//! LiveKit room event handling.
//!
//! Processes room state changes and participant updates.

use std::sync::Arc;

use slint::{ComponentHandle, Model, ModelRc, VecModel};

use chatter_core::client::ChatterClient;
use chatter_core::calls::LiveKitRoomEvent;
use crate::bin_ui::converters::get_initials;
use crate::{CallParticipantData, MainWindow};

/// The local participant is pinned to the LAST tile (the layout treats the last
/// row as "You" and feeds it `local-video-frame`). Split it off before touching
/// the remote rows. Callers must re-attach it via [`order_participants`].
fn split_local(mut participants: Vec<CallParticipantData>) -> (Vec<CallParticipantData>, Option<CallParticipantData>) {
    let local = if participants.is_empty() { None } else { Some(participants.remove(participants.len() - 1)) };
    (participants, local)
}

/// Rebuild the participant list: remotes ascending by user_id, local pinned last.
/// The GL frame list is published in ascending participant-id order and matched
/// to tiles positionally (see `gl_call.rs`), so the remote rows must be sorted
/// the same way or a peer's frame lands on the wrong tile.
fn order_participants(mut remotes: Vec<CallParticipantData>, local: Option<CallParticipantData>) -> Vec<CallParticipantData> {
    remotes.sort_by(|a, b| a.user_id.as_str().cmp(b.user_id.as_str()));
    if let Some(l) = local {
        remotes.push(l);
    }
    remotes
}

/// Wire room events to update UI (participant count, etc.).
pub fn wire_room_events(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak = window.as_weak();
    let ui_ctx = ui_ctx.clone();

    // Spawn a tokio task to consume room events (tokio runtime needed for channels and sleep)
    tokio::spawn(async move {
        // Wait for call service to be initialized (probed via the orchestrator
        // broadcast channel — same OnceLock), then take the room events receiver.
        while ui_ctx.subscribe_call_orchestrator_events().is_none() {
            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        }

        let Some(mut rx) = ui_ctx.take_call_room_events() else {
            tracing::warn!("Room events receiver already taken");
            return;
        };

        tracing::info!("Started room events listener");

        while let Some(event) = rx.recv().await {
            tracing::debug!("UI received room event: {:?}", event);
            let weak = weak.clone();

            match event {
                LiveKitRoomEvent::StateChanged(state) => {
                    if let chatter_core::calls::RoomState::Connected { participant_count, .. } = state {
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(w) = weak.upgrade() {
                                let mut data = w.get_active_call_data();
                                data.participant_count = participant_count as i32;
                                w.set_active_call_data(data);
                            }
                        });
                    }
                }
                LiveKitRoomEvent::ParticipantJoined { identity, display_name } => {
                    tracing::info!("Participant joined: {} ({:?})", identity, display_name);
                    let display = display_name.clone().unwrap_or_else(|| identity.clone());
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            let mut data = w.get_active_call_data();

                            // Check if participant already exists (e.g. pre-populated placeholder)
                            let existing: Vec<CallParticipantData> =
                                (0..data.participants.row_count())
                                    .map(|i| data.participants.row_data(i).unwrap_or_default())
                                    .collect();
                            let already_has_remote = existing.iter().any(|p| {
                                let uid = p.user_id.as_str();
                                uid == identity.as_str() || (uid != "You" && uid != identity.as_str())
                            });
                            if already_has_remote {
                                // Update the existing placeholder with the real identity
                                let mut participants = existing;
                                for p in &mut participants {
                                    let uid = p.user_id.as_str();
                                    if uid != "You" && uid != identity.as_str() {
                                        // Replace placeholder (e.g. "Anonymous") with real identity
                                        p.user_id = identity.clone().into();
                                        p.display_name = display.clone().into();
                                        p.initials = get_initials(&display).into();
                                        break;
                                    }
                                }
                                data.participants = ModelRc::new(VecModel::from(participants));
                                w.set_active_call_data(data);
                                tracing::debug!("UI updated existing participant placeholder");
                                return;
                            }

                            data.participant_count += 1;

                            // Add participant to list
                            let new_initials = get_initials(&display);
                            let new_participant = CallParticipantData {
                                user_id: identity.into(),
                                display_name: display.into(),
                                initials: new_initials.into(),
                                avatar_source: slint::Image::default(),
                                is_muted: false,
                                is_video_enabled: false,
                                is_screen_sharing: false,
                                is_speaking: false,
                                is_audio_muted_locally: false,
                                is_video_paused_locally: false,
                            };

                            let mut participants = existing;
                            // Insert remote participants at the beginning, keeping local participant last
                            // This matches the Slint layout which assumes remote participants come first
                            participants.insert(0, new_participant);
                            data.participants = ModelRc::new(VecModel::from(participants));

                            w.set_active_call_data(data);
                            tracing::debug!("UI updated for participant join");
                        }
                    });
                }
                LiveKitRoomEvent::ParticipantLeft { identity } => {
                    tracing::info!("Participant left: {}", identity);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            let mut data = w.get_active_call_data();
                            data.participant_count = (data.participant_count - 1).max(0);

                            // Remove participant from list
                            let participants: Vec<CallParticipantData> =
                                (0..data.participants.row_count())
                                    .filter_map(|i| {
                                        let p = data.participants.row_data(i)?;
                                        if p.user_id.to_string() != identity {
                                            Some(p)
                                        } else {
                                            None
                                        }
                                    })
                                    .collect();
                            data.participants = ModelRc::new(VecModel::from(participants));

                            w.set_active_call_data(data);
                            tracing::debug!("UI updated for participant leave");
                        }
                    });
                    // Key rotation on participant leave is driven by the core
                    // room-events policy listener (see `ChatterClient::start`).
                }
                LiveKitRoomEvent::TrackSubscribed { participant_identity, track_sid, is_video, is_screen_share } => {
                    tracing::info!(
                        "Track subscribed: {} from {} (video: {}, screen_share: {})",
                        track_sid, participant_identity, is_video, is_screen_share
                    );

                    if !is_video {
                        continue;
                    }
                    let identity = participant_identity.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        let Some(w) = weak.upgrade() else { return };
                        let mut data = w.get_active_call_data();
                        let all: Vec<CallParticipantData> =
                            (0..data.participants.row_count())
                                .map(|i| data.participants.row_data(i).unwrap_or_default())
                                .collect();
                        // Separate local BEFORE reordering remotes, else the sort
                        // would shuffle "You" in among the peers and misalign every
                        // frame slot.
                        let (mut remotes, local) = split_local(all);

                        if is_screen_share {
                            // Screen share is its OWN tile (camera + screen are
                            // separate tracks from the same peer — one tile would
                            // flash between them). Add a pseudo-participant keyed by
                            // the same synthetic id the frame store uses.
                            let screen_id =
                                chatter_core::calls::screen_share_frame_id(&identity, true);
                            if remotes.iter().any(|p| p.user_id.to_string() == screen_id) {
                                return;
                            }
                            let (name, initials) = remotes
                                .iter()
                                .find(|p| p.user_id.to_string() == identity)
                                .map(|p| (p.display_name.to_string(), p.initials.to_string()))
                                .unwrap_or_else(|| (identity.clone(), get_initials(&identity)));
                            remotes.push(CallParticipantData {
                                user_id: screen_id.into(),
                                display_name: format!("{name} (screen)").into(),
                                initials: initials.into(),
                                avatar_source: slint::Image::default(),
                                is_muted: false,
                                is_video_enabled: true,
                                is_screen_sharing: true,
                                is_speaking: false,
                                is_audio_muted_locally: false,
                                is_video_paused_locally: false,
                            });
                        } else {
                            // Camera track — light up the real participant's tile.
                            for p in &mut remotes {
                                if p.user_id.to_string() == identity {
                                    p.is_video_enabled = true;
                                }
                            }
                        }

                        let participants = order_participants(remotes, local);
                        data.participant_count = participants.len() as i32;
                        data.participants = ModelRc::new(VecModel::from(participants));
                        w.set_active_call_data(data);
                    });
                }
                LiveKitRoomEvent::TrackUnsubscribed {
                    participant_identity, track_sid, is_video, is_screen_share,
                } => {
                    tracing::info!(
                        "Track unsubscribed: {} from {} (video={}, screen_share={})",
                        track_sid, participant_identity, is_video, is_screen_share
                    );

                    if !is_video {
                        // Audio-only unsubscribe — UI tile state unchanged.
                        continue;
                    }

                    let identity = participant_identity.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        let Some(w) = weak.upgrade() else { return };
                        let mut data = w.get_active_call_data();
                        let all: Vec<CallParticipantData> =
                            (0..data.participants.row_count())
                                .map(|i| data.participants.row_data(i).unwrap_or_default())
                                .collect();
                        let (mut remotes, local) = split_local(all);

                        if is_screen_share {
                            // Screen share stopped — drop its whole pseudo-tile.
                            let screen_id =
                                chatter_core::calls::screen_share_frame_id(&identity, true);
                            remotes.retain(|p| p.user_id.to_string() != screen_id);
                        } else {
                            // Camera track unsubscribed — clear the real tile's video flag.
                            for p in &mut remotes {
                                if p.user_id.to_string() == identity {
                                    p.is_video_enabled = false;
                                }
                            }
                        }

                        let participants = order_participants(remotes, local);
                        data.participant_count = participants.len() as i32;
                        data.participants = ModelRc::new(VecModel::from(participants));
                        w.set_active_call_data(data);
                        tracing::debug!(
                            "Cleared video state for {} (screen_share={})",
                            identity, is_screen_share
                        );
                    });
                }
                LiveKitRoomEvent::EncryptionStateChanged { participant_identity, is_encrypted } => {
                    tracing::debug!(
                        "Encryption state changed: {} - encrypted: {}",
                        participant_identity, is_encrypted
                    );
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            let mut data = w.get_active_call_data();
                            data.is_encrypted = is_encrypted;
                            w.set_active_call_data(data);
                        }
                    });
                }
                LiveKitRoomEvent::ConnectionQualityChanged { participant_identity, quality } => {
                    // UI doesn't surface per-participant quality today.
                    // Log so SDK users see the stream is live.
                    tracing::debug!(
                        "Connection quality: {} -> {}",
                        participant_identity, quality
                    );
                }
            }
        }

        tracing::info!("Room events listener stopped");
    });
}

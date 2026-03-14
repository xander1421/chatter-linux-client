//! LiveKit room event handling.
//!
//! Processes room state changes and participant updates.

use std::sync::Arc;

use slint::{ComponentHandle, Model, ModelRc, VecModel};

use chatter_linux::app::UIContext;
use chatter_linux::calls::LiveKitRoomEvent;
use crate::{CallParticipantData, MainWindow};

/// Wire room events to update UI (participant count, etc.).
pub fn wire_room_events(window: &MainWindow, ui_ctx: &Arc<UIContext>) {
    let weak = window.as_weak();
    let call_service = ui_ctx.call_service.clone();

    // Spawn a tokio task to consume room events (tokio runtime needed for channels and sleep)
    tokio::spawn(async move {
        // Wait for call service to be initialized
        let service = loop {
            if let Some(svc) = call_service.get() {
                break svc.clone();
            }
            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        };

        // Take the room events receiver
        let Some(mut rx) = service.take_room_events() else {
            tracing::warn!("Room events receiver already taken");
            return;
        };

        tracing::info!("Started room events listener");

        while let Some(event) = rx.recv().await {
            tracing::debug!("UI received room event: {:?}", event);
            let weak = weak.clone();

            match event {
                LiveKitRoomEvent::StateChanged(state) => {
                    if let chatter_linux::calls::RoomState::Connected { participant_count, .. } = state {
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
                                    .map(|i| data.participants.row_data(i).unwrap())
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
                            let new_participant = CallParticipantData {
                                user_id: identity.into(),
                                display_name: display.into(),
                                avatar_url: "".into(),
                                is_muted: false,
                                is_video_enabled: false,
                                is_screen_sharing: false,
                                is_speaking: false,
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
                }
                LiveKitRoomEvent::TrackSubscribed { participant_identity, track_sid, is_video, is_screen_share } => {
                    tracing::info!(
                        "Track subscribed: {} from {} (video: {}, screen_share: {})",
                        track_sid, participant_identity, is_video, is_screen_share
                    );

                    // Update participant's video/screen share state
                    if is_video {
                        let identity = participant_identity.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(w) = weak.upgrade() {
                                let mut data = w.get_active_call_data();
                                let participants: Vec<CallParticipantData> =
                                    (0..data.participants.row_count())
                                        .map(|i| {
                                            let mut p = data.participants.row_data(i).unwrap();
                                            if p.user_id.to_string() == identity {
                                                // Always set is_video_enabled for any video track
                                                // (camera or screen share) so the tile shows video
                                                p.is_video_enabled = true;
                                                if is_screen_share {
                                                    p.is_screen_sharing = true;
                                                }
                                            }
                                            p
                                        })
                                        .collect();
                                data.participants = ModelRc::new(VecModel::from(participants));
                                w.set_active_call_data(data);
                                tracing::debug!(
                                    "Updated participant {} video_enabled=true screen_sharing={}",
                                    identity, is_screen_share
                                );
                            }
                        });
                    }
                }
                LiveKitRoomEvent::TrackUnsubscribed { participant_identity, track_sid } => {
                    tracing::info!(
                        "Track unsubscribed: {} from {}",
                        track_sid, participant_identity
                    );
                    // Note: We don't know if this was video or audio track
                    // A more robust solution would include track type in the event
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
            }
        }

        tracing::info!("Room events listener stopped");
    });
}

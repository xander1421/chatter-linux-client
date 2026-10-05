//! LiveKit room event handling.
//!
//! Processes room state changes and participant updates.

use std::sync::Arc;

use slint::{ComponentHandle, Model, ModelRc, VecModel};

use chatter_core::client::ChatterClient;
use chatter_core::calls::{tiles, LiveKitRoomEvent};
use chatter_core::ui::initials_from_name;
use crate::{CallParticipantData, MainWindow};

/// Core's tile order ([`chatter_core::calls::tiles::order_tiles`]).
fn order_participants(remotes: Vec<CallParticipantData>, local: Option<CallParticipantData>) -> Vec<CallParticipantData> {
    tiles::order_tiles(remotes, local, |p| p.user_id.as_str())
}

/// Render core membership, preserving media flags and anonymous peer presentation.
pub(super) fn reconcile_roster(
    data: &mut crate::ActiveCallData,
    roster: Vec<(String, chatter_core::ui::chat_data::CallParticipantInfo)>,
) {
    let all = (0..data.participants.row_count())
        .filter_map(|i| data.participants.row_data(i)).collect();
    let (mut previous, local) = tiles::split_local(all);
    let mut remotes = Vec::new();
    let count = roster.len() + usize::from(local.is_some());
    for (identity, info) in &roster {
        let existing = previous.iter().position(|p| p.user_id.as_str() == identity);
        let anonymous_slot = if data.is_anonymous_call && existing.is_none() {
            previous.iter().position(|p| p.user_id.is_empty())
        } else { None };
        let mut tile = existing.or(anonymous_slot)
            .map(|i| previous.remove(i)).unwrap_or_default();
        tile.user_id = identity.clone().into();
        if !data.is_anonymous_call || tile.display_name.is_empty() {
            tile.display_name = info.display_name.clone().into();
            tile.initials = info.initials.clone().into();
            tile.avatar_source = info.avatar_bytes.as_deref()
                .and_then(crate::bin_ui::converters::avatar_bytes_to_image)
                .unwrap_or_default();
        }
        remotes.push(tile);
    }
    for tile in previous {
        if tile.is_screen_sharing && chatter_core::client::calls::room_roster_keeps_screen(
            tile.user_id.as_str(), tile.display_name.as_str(), &roster, local.as_ref().map(|p| p.user_id.as_str()),
        ) {
            remotes.push(tile);
        }
    }
    data.participant_count = count as i32;
    data.participants = ModelRc::new(VecModel::from(order_participants(remotes, local)));
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
                LiveKitRoomEvent::StateChanged(_)
                | LiveKitRoomEvent::ParticipantJoined { .. }
                | LiveKitRoomEvent::ParticipantLeft { .. } => {
                    // Events are wakeups, not membership decisions: mount may
                    // have overwritten earlier joins, and departures may lag.
                    let Some(snapshot) = ui_ctx.call_room_snapshot().await else { continue };
                    let _ = slint::invoke_from_event_loop(move || {
                        let Some(w) = weak.upgrade() else { return };
                        let mut data = w.get_active_call_data();
                        let matches = if data.is_anonymous_call {
                            data.channel_id.as_str() == snapshot.call.channel_id.to_string()
                        } else {
                            data.call_id.as_str() == snapshot.call.id.to_string()
                                && data.channel_id.as_str() == snapshot.call.channel_id.to_string()
                        };
                        if !matches || !w.get_active_call_visible() { return; }
                        if let Some(remotes) = snapshot.remotes {
                            reconcile_roster(&mut data, remotes);
                            w.set_active_call_data(data);
                        }
                    });
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
                        let (mut remotes, local) = tiles::split_local(all);

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
                                .unwrap_or_else(|| (identity.clone(), initials_from_name(&identity)));
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
                        let (mut remotes, local) = tiles::split_local(all);

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
                LiveKitRoomEvent::ActiveSpeakersChanged { speaker_identities } => {
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            let mut data = w.get_active_call_data();
                            let updated: Vec<CallParticipantData> =
                                (0..data.participants.row_count())
                                    .map(|i| {
                                        let mut p = data.participants.row_data(i).unwrap_or_default();
                                        p.is_speaking = speaker_identities
                                            .iter()
                                            .any(|id| id == p.user_id.as_str());
                                        p
                                    })
                                    .collect();
                            data.participants = ModelRc::new(VecModel::from(updated));
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

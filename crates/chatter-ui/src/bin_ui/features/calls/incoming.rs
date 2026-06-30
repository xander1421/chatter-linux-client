//! Incoming call handling.
//!
//! Thin UI glue around the client's `accept_presented_incoming_call` /
//! `decline_presented_incoming_call` methods. All orchestration (modal
//! policy, active-slot replacement, wire-level accept/decline) lives in
//! [`chatter_core::services::call_worker`] and [`chatter_core::client`].
//! This file only:
//! - subscribes to orchestrator broadcast events → repaints the modal
//! - translates accept/decline button clicks into client calls
//! - renders the `ActiveCallData` Slint struct from the client's return value

use std::sync::Arc;

use slint::{ComponentHandle, Model, ModelRc, VecModel};

use chatter_core::client::ChatterClient;
use chatter_core::client::calls::AcceptedIncomingCall;
use chatter_core::services::call_worker::{
    CallEndReason, CallOrchestratorEvent, OrchestratedCallKind,
};
use crate::{ActiveCallData, CallParticipantData, MainWindow};
use crate::bin_ui::converters::{avatar_bytes_to_image, get_initials};

use super::timers::{start_call_timer, start_video_frame_timer};

/// Wire incoming call accept/decline callbacks.
pub fn wire_incoming_call(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak = window.as_weak();
    let ctx_for_accept = ui_ctx.clone();
    let ctx_for_decline = ui_ctx.clone();

    window.on_accept_incoming_call({
        let weak = weak.clone();
        move |call_id| {
            let call_id_str = call_id.to_string();
            let weak = weak.clone();
            let ui_ctx = ctx_for_accept.clone();

            let replaces_active = weak
                .upgrade()
                .map(|w| w.get_incoming_call_replaces_active())
                .unwrap_or(false);

            // Hide the modal immediately — the accept flow paints the
            // active-call view directly on success.
            if let Some(w) = weak.upgrade() {
                w.set_incoming_call_visible(false);
                w.set_incoming_call_replaces_active(false);
            }

            tokio::spawn(async move {
                match ui_ctx
                    .accept_presented_incoming_call(&call_id_str, replaces_active)
                    .await
                {
                    Ok(accepted) => render_accepted_call(&weak, &ui_ctx, accepted),
                    Err(e) => {
                        tracing::error!(error = %e, "Failed to accept incoming call");
                    }
                }
            });
        }
    });

    window.on_decline_incoming_call({
        let weak = weak.clone();
        move |call_id| {
            let call_id_str = call_id.to_string();
            let ui_ctx = ctx_for_decline.clone();

            if let Some(w) = weak.upgrade() {
                w.set_incoming_call_visible(false);
                w.set_incoming_call_replaces_active(false);
            }

            tokio::spawn(async move {
                if let Err(e) = ui_ctx
                    .decline_presented_incoming_call(&call_id_str)
                    .await
                {
                    tracing::debug!(error = %e, "Decline call notification failed");
                }
            });
        }
    });
}

/// Paint the Slint active-call view from the client's accept result.
fn render_accepted_call(
    weak: &slint::Weak<MainWindow>,
    ui_ctx: &Arc<ChatterClient>,
    accepted: AcceptedIncomingCall,
) {
    // Timers use a cheap Arc-backed handle to shared video frames.
    let shared_frames = match ui_ctx.call_video_frames() {
        Some(f) => f,
        None => {
            tracing::error!("CallService not initialized after accept");
            return;
        }
    };

    let current_user_id = ui_ctx
        .session_current_user_id()
        .map(|id| id.to_string())
        .unwrap_or_default();
    let my_display_name = ui_ctx
        .my_display_name()
        .unwrap_or_else(|| "You".to_string());

    match accepted {
        AcceptedIncomingCall::Dm {
            setup,
            peer_id,
            peer_name,
            is_video,
        } => {
            let weak = weak.clone();
            let frames = shared_frames.clone();
            let _ = slint::invoke_from_event_loop(move || {
                let Some(w) = weak.upgrade() else { return };
                let local_initials = get_initials(&my_display_name);
                let remote_initials = get_initials(&peer_name);
                let local_participant = CallParticipantData {
                    user_id: current_user_id.clone().into(),
                    display_name: my_display_name.clone().into(),
                    initials: local_initials.into(),
                    avatar_source: w.get_user_avatar_source(),
                    is_muted: false,
                    is_video_enabled: is_video,
                    is_screen_sharing: false,
                    is_speaking: false,
                    is_audio_muted_locally: false,
                    is_video_paused_locally: false,
                };
                let remote_participant = CallParticipantData {
                    user_id: peer_id.to_string().into(),
                    display_name: peer_name.clone().into(),
                    initials: remote_initials.into(),
                    avatar_source: slint::Image::default(),
                    is_muted: false,
                    is_video_enabled: is_video,
                    is_screen_sharing: false,
                    is_speaking: false,
                    is_audio_muted_locally: false,
                    is_video_paused_locally: false,
                };
                let call_data = ActiveCallData {
                    call_id: setup.call_id.to_string().into(),
                    channel_id: setup.channel_id.to_string().into(),
                    title: "Call".into(),
                    is_video_call: is_video,
                    is_anonymous_call: false,
                    is_encrypted: true,
                    is_identity_verified: false,
                    identity_warning: "".into(),
                    participant_count: 2,
                    duration_seconds: 0,
                    participants: ModelRc::new(VecModel::from(vec![
                        remote_participant,
                        local_participant,
                    ])),
                    verification_emoji_1: setup.verification_emojis[0].clone().into(),
                    verification_emoji_2: setup.verification_emojis[1].clone().into(),
                    verification_emoji_3: setup.verification_emojis[2].clone().into(),
                    verification_emoji_4: setup.verification_emojis[3].clone().into(),
                };
                w.set_active_call_data(call_data);
                w.set_call_is_muted(false);
                w.set_call_is_video_enabled(is_video);
                w.set_call_is_screen_sharing(false);
                w.set_active_call_visible(true);
                start_call_timer(&w, None);
                start_video_frame_timer(&w, frames);
            });
            // Media is auto-published by the call worker after connect.
        }

        AcceptedIncomingCall::Anon { setup, peer_id: _ } => {
            render_active_anon_call(
                weak,
                ui_ctx,
                shared_frames,
                setup.room_name,
                setup.channel_id.to_string(),
                setup.peer_display_name,
                setup.is_video,
                setup.verification_emojis,
                setup.started_at_ns,
                setup.peer_avatar,
            );
        }
    }
}

/// Mount the active anonymous call UI.
///
/// Shared between the "accept incoming" path (`AcceptedIncomingCall::Anon`)
/// and the orchestrator's `AnonCallConnected` event (caller-side answer
/// received). Both paths need the same UI state, so keep them in one helper.
/// Media publication is handled inside the call worker — this is pure view.
#[allow(clippy::too_many_arguments)]
fn render_active_anon_call(
    weak: &slint::Weak<MainWindow>,
    _ui_ctx: &Arc<ChatterClient>,
    shared_frames: chatter_core::calls::SharedVideoFrames,
    room_name: String,
    channel_id: String,
    peer_display_name: String,
    is_video: bool,
    verification_emojis: [String; 4],
    started_at_ns: u64,
    peer_avatar: Option<Vec<u8>>,
) {
    let weak = weak.clone();
    let frames = shared_frames.clone();
    let _ = slint::invoke_from_event_loop(move || {
        let Some(w) = weak.upgrade() else { return };
        // If `LiveKitRoomEvent::ParticipantJoined` already landed on the UI
        // thread (it races AnonCallConnected via two independent
        // `invoke_from_event_loop` dispatches), the remote slot already
        // carries the server-assigned identity (e.g. "anon-c1954f…").
        // Re-stamping a literal "Anonymous" placeholder here clobbers it,
        // and subsequent `TrackSubscribed` events can't match the
        // identity back to a participant — the screen-share tile then
        // stays in is_video_enabled=false until the user does pause+
        // unpause to force a flag flip.
        let existing = w.get_active_call_data().participants;
        let preserved_remote_user_id: Option<slint::SharedString> = (0..existing.row_count())
            .filter_map(|i| existing.row_data(i))
            .find(|p| {
                let uid = p.user_id.as_str();
                !uid.is_empty() && uid != "You" && uid != "Anonymous"
            })
            .map(|p| p.user_id);

        let local_participant = CallParticipantData {
            user_id: "You".into(),
            display_name: "You".into(),
            initials: "Y".into(),
            avatar_source: w.get_user_avatar_source(),
            is_muted: false,
            is_video_enabled: is_video,
            is_screen_sharing: false,
            is_speaking: false,
            is_audio_muted_locally: false,
            is_video_paused_locally: false,
        };
        let remote_initials = get_initials(&peer_display_name);
        let remote_avatar_source = peer_avatar
            .as_deref()
            .and_then(avatar_bytes_to_image)
            .unwrap_or_default();
        let remote_participant = CallParticipantData {
            user_id: preserved_remote_user_id.unwrap_or_else(|| "Anonymous".into()),
            display_name: peer_display_name.into(),
            initials: remote_initials.into(),
            avatar_source: remote_avatar_source,
            is_muted: false,
            is_video_enabled: is_video,
            is_screen_sharing: false,
            is_speaking: false,
            is_audio_muted_locally: false,
            is_video_paused_locally: false,
        };
        let call_data = ActiveCallData {
            call_id: room_name.into(),
            channel_id: channel_id.into(),
            title: "Anonymous Call".into(),
            is_video_call: is_video,
            is_anonymous_call: true,
            is_encrypted: true,
            is_identity_verified: true,
            identity_warning: "".into(),
            participant_count: 2,
            duration_seconds: 0,
            participants: ModelRc::new(VecModel::from(vec![
                remote_participant,
                local_participant,
            ])),
            verification_emoji_1: verification_emojis[0].clone().into(),
            verification_emoji_2: verification_emojis[1].clone().into(),
            verification_emoji_3: verification_emojis[2].clone().into(),
            verification_emoji_4: verification_emojis[3].clone().into(),
        };
        w.set_active_call_data(call_data);
        w.set_call_is_muted(false);
        w.set_call_is_video_enabled(is_video);
        w.set_call_is_screen_sharing(false);
        w.set_active_call_visible(true);
        super::super::auth::initial_data::hide_incoming_call(&w);
        start_call_timer(&w, Some(started_at_ns));
        start_video_frame_timer(&w, frames);
    });
    // Media is auto-published by the call worker after connect — no UI-side
    // publish needed.
}

/// Subscribe to CallOrchestrator broadcast events and translate them into
/// Slint property updates. The orchestrator (in core) owns the policy
/// decision — this function only paints the modal.
pub fn wire_orchestrator_events(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let ui_ctx = ui_ctx.clone();
    let weak = window.as_weak();

    slint::spawn_local(async move {
        let mut rx = loop {
            if let Some(rx) = ui_ctx.subscribe_call_orchestrator_events() {
                break rx;
            }
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        };

        tracing::info!("Call orchestrator UI subscriber started");

        loop {
            match rx.recv().await {
                Ok(event) => handle_orchestrator_event(&weak, &ui_ctx, event),
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!(skipped = n, "Call orchestrator UI subscriber lagged");
                    continue;
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    tracing::info!("Call orchestrator UI subscriber stopping — channel closed");
                    break;
                }
            }
        }
    })
    .expect("spawn call orchestrator subscriber");
}

fn handle_orchestrator_event(
    weak: &slint::Weak<MainWindow>,
    ui_ctx: &Arc<ChatterClient>,
    event: CallOrchestratorEvent,
) {
    match event {
        CallOrchestratorEvent::IncomingPresented {
            call,
            kind,
            peer_id,
            peer_name,
            is_video,
            is_identity_verified,
            identity_warning,
            replaces_active,
            ..
        } => {
            let weak = weak.clone();
            let call_id = call.as_str().to_string();
            let peer_id_str = peer_id.to_string();
            let call_type_str = if is_video { "video".to_string() } else { "voice".to_string() };
            let is_anonymous = matches!(
                kind,
                OrchestratedCallKind::AnonDm | OrchestratedCallKind::AnonGroup
            );

            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak.upgrade() {
                    super::super::auth::initial_data::show_incoming_call(
                        &w,
                        &call_id,
                        &peer_id_str,
                        &peer_name,
                        &call_type_str,
                        is_identity_verified,
                        &identity_warning,
                        is_anonymous,
                    );
                    w.set_incoming_call_replaces_active(replaces_active);
                }
            });
        }

        CallOrchestratorEvent::IncomingWithdrawn { call, reason } => {
            tracing::info!(
                call = %call.as_str(),
                reason = ?reason,
                "Incoming call withdrawn"
            );
            let weak = weak.clone();
            let expected_call_id = call.as_str().to_string();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak.upgrade() {
                    let current = w.get_incoming_call_data().call_id.to_string();
                    if current == expected_call_id {
                        super::super::auth::initial_data::hide_incoming_call(&w);
                        w.set_incoming_call_replaces_active(false);
                    }
                }
            });
        }

        CallOrchestratorEvent::ActiveEnded { call, reason } => {
            tracing::info!(
                call = %call.as_str(),
                reason = ?reason,
                "Active call ended"
            );
            let should_clear = matches!(
                reason,
                CallEndReason::ReplacedByIncoming
                    | CallEndReason::Declined
                    | CallEndReason::Error(_)
            );
            if should_clear {
                let weak = weak.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(w) = weak.upgrade() {
                        w.set_active_call_visible(false);
                        super::super::auth::initial_data::hide_incoming_call(&w);
                    }
                });
            }
        }

        CallOrchestratorEvent::OutgoingStarted { call, .. }
        | CallOrchestratorEvent::ActiveConnected { call, .. } => {
            tracing::debug!(call = %call.as_str(), "Orchestrator state transition");
        }

        CallOrchestratorEvent::AnonCallConnected { call, info, .. } => {
            tracing::info!(
                call = %call.as_str(),
                "Anon call connected — mounting active-call UI"
            );
            let Some(frames) = ui_ctx.call_video_frames() else {
                tracing::error!("CallService not initialized — cannot render anon call");
                return;
            };
            render_active_anon_call(
                weak,
                ui_ctx,
                frames,
                info.room_name,
                info.channel_id.to_string(),
                info.peer_display_name,
                info.is_video,
                info.verification_emojis,
                info.started_at_ns,
                info.peer_avatar,
            );
        }
    }
}

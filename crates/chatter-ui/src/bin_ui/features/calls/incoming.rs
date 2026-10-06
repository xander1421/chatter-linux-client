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

use crate::bin_ui::converters::avatar_bytes_to_image;
use crate::{ActiveCallData, CallParticipantData, MainWindow};
use chatter_core::client::ChatterClient;
use chatter_core::client::calls::AcceptedIncomingCall;
use chatter_core::services::call_worker::{
    CallEndReason, CallOrchestratorEvent, OrchestratedCallKind,
};
use chatter_core::ui::initials_from_name;

use super::timers::{
    start_call_timer, start_video_frame_timer, stop_call_timer, stop_video_frame_timer,
};

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
            #[cfg(target_os = "android")]
            let is_video = weak
                .upgrade()
                .map(|w| w.get_incoming_call_data().is_video_call)
                .unwrap_or(false);

            // Swap the modal for the call view now; the accept flow repaints
            // it with the real call on success.
            if let Some(w) = weak.upgrade() {
                show_answering(&w);
                w.set_incoming_call_visible(false);
                w.set_incoming_call_replaces_active(false);
            }

            slint::spawn_local(async move {
                // Answering without the microphone joins a call nobody can
                // hear, so wait for the prompt's answer first (see
                // outgoing.rs). Refused: say why and decline, rather than
                // leave the caller ringing a modal that is already gone.
                #[cfg(target_os = "android")]
                {
                    use crate::platform::android::permissions;
                    let mut perms = vec![permissions::RECORD_AUDIO];
                    if is_video {
                        perms.push(permissions::CAMERA);
                    }
                    permissions::ensure(&perms).await;
                    if !permissions::has(permissions::RECORD_AUDIO) {
                        super::outgoing::fail_call(&weak, super::outgoing::MIC_DENIED);
                        tokio::spawn(async move {
                            let _ = ui_ctx.decline_presented_incoming_call(&call_id_str).await;
                        });
                        return;
                    }
                }
                tokio::spawn(accept(weak, ui_ctx, call_id_str, replaces_active));
            })
            .unwrap();
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
                if let Err(e) = ui_ctx.decline_presented_incoming_call(&call_id_str).await {
                    tracing::debug!(error = %e, "Decline call notification failed");
                }
            });
        }
    });
}

async fn accept(
    weak: slint::Weak<MainWindow>,
    ui_ctx: Arc<ChatterClient>,
    call_id: String,
    replaces_active: bool,
) {
    match ui_ctx
        .accept_presented_incoming_call(&call_id, replaces_active)
        .await
    {
        Ok(accepted) => render_accepted_call(&weak, &ui_ctx, accepted),
        Err(e) => {
            tracing::error!(error = %e, "Failed to accept incoming call");
            // The answering view went up on the tap; take it down and say why.
            let _ = slint::invoke_from_event_loop(move || {
                super::outgoing::fail_call(&weak, super::outgoing::CALL_FAILED);
            });
        }
    }
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
            // Resolve the peer's avatar from local state (same source the DM
            // sidebar uses) so the pre-populated remote tile carries it. Without
            // this the placeholder shows default initials, and this
            // set_active_call_data can race-clobber the avatar ParticipantJoined
            // would otherwise install — leaving the caller's tile blank.
            let peer_avatar_bytes = ui_ctx
                .resolve_call_participant(&peer_id.to_string())
                .avatar_bytes;
            let ui_ctx = ui_ctx.clone();
            let _ = slint::invoke_from_event_loop(move || {
                let Some(w) = weak.upgrade() else { return };
                let local_initials = initials_from_name(&my_display_name);
                let remote_initials = initials_from_name(&peer_name);
                let remote_avatar = peer_avatar_bytes
                    .as_deref()
                    .and_then(avatar_bytes_to_image)
                    .unwrap_or_default();
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
                    avatar_source: remote_avatar,
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
                    // Who the call is with — what the return-to-call bar shows.
                    title: peer_name.clone().into(),
                    is_video_call: is_video,
                    is_anonymous_call: false,
                    is_encrypted: true,
                    is_identity_verified: false,
                    identity_warning: "".into(),
                    participant_count: 2,
                    duration_seconds: 0,
                    is_connected: true,
                    participants: ModelRc::new(VecModel::from(vec![
                        remote_participant,
                        local_participant,
                    ])),
                    verification_key_version: "".into(),
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
                start_call_timer(&w, ui_ctx.clone());
                start_video_frame_timer(&w, frames);
                open_call_screen(&w);
            });
            // Media is auto-published by the call worker after connect.
        }

        AcceptedIncomingCall::Group { setup, title } => {
            let weak = weak.clone();
            let frames = shared_frames.clone();
            let ui_ctx = ui_ctx.clone();
            let _ = slint::invoke_from_event_loop(move || {
                let Some(w) = weak.upgrade() else { return };
                // Keep the remote tiles already up: the answering view's
                // caller tile, and any `ParticipantJoined` for the people
                // already in the room that landed before this repaint. Each
                // later joiner adds its own tile (room_events.rs).
                let existing = w.get_active_call_data().participants;
                let mut participants: Vec<CallParticipantData> = existing.iter().collect();
                participants.pop(); // the local tile, pinned last; rebuilt below
                participants.push(CallParticipantData {
                    user_id: current_user_id.clone().into(),
                    display_name: my_display_name.clone().into(),
                    initials: initials_from_name(&my_display_name).into(),
                    avatar_source: w.get_user_avatar_source(),
                    is_muted: false,
                    is_video_enabled: setup.is_video,
                    is_screen_sharing: false,
                    is_speaking: false,
                    is_audio_muted_locally: false,
                    is_video_paused_locally: false,
                });
                let call_data = ActiveCallData {
                    call_id: setup.call_id.to_string().into(),
                    channel_id: setup.channel_id.to_string().into(),
                    title: title.into(),
                    is_video_call: setup.is_video,
                    is_anonymous_call: false,
                    is_encrypted: true,
                    is_identity_verified: false,
                    identity_warning: "".into(),
                    participant_count: participants.len() as i32,
                    duration_seconds: 0,
                    is_connected: true,
                    participants: ModelRc::new(VecModel::from(participants)),
                    verification_key_version: "".into(),
                    verification_emoji_1: setup.verification_emojis[0].clone().into(),
                    verification_emoji_2: setup.verification_emojis[1].clone().into(),
                    verification_emoji_3: setup.verification_emojis[2].clone().into(),
                    verification_emoji_4: setup.verification_emojis[3].clone().into(),
                };
                w.set_active_call_data(call_data);
                w.set_call_is_muted(false);
                w.set_call_is_video_enabled(setup.is_video);
                w.set_call_is_screen_sharing(false);
                w.set_active_call_visible(true);
                start_call_timer(&w, ui_ctx.clone());
                start_video_frame_timer(&w, frames);
                open_call_screen(&w);
            });
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
                setup.peer_avatar,
            );
            // Queued after the render above (the event loop runs them in order).
            let weak = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak.upgrade() {
                    open_call_screen(&w);
                }
            });
        }
    }
}

/// Put the call view up the moment Accept is tapped, from the modal's own
/// data. Answering waits on the mic prompt, the answer send and the LiveKit
/// join — seconds on a good network, the 10 s connect timeout on a bad one —
/// and until now all of it rendered as the chat list, which read as the call
/// having been dropped. `render_accepted_call` / `render_active_anon_call`
/// overwrite this with the real call; a failure takes it down (`fail_call`).
fn show_answering(w: &MainWindow) {
    let incoming = w.get_incoming_call_data();
    let is_video = incoming.is_video_call;
    let my_display_name = w.get_user_display_name();
    let participant = |user_id: slint::SharedString,
                       display_name: slint::SharedString,
                       initials: slint::SharedString,
                       avatar_source: slint::Image| CallParticipantData {
        user_id,
        display_name,
        initials,
        avatar_source,
        is_muted: false,
        is_video_enabled: is_video,
        is_screen_sharing: false,
        is_speaking: false,
        is_audio_muted_locally: false,
        is_video_paused_locally: false,
    };
    // An anon peer's room identity is unknown until it joins; an empty id is
    // the slot `ParticipantJoined` adopts (see `render_active_anon_call`).
    let remote_id = if incoming.is_anonymous_call {
        "".into()
    } else {
        incoming.caller_id.clone()
    };
    let remote = participant(
        remote_id,
        incoming.caller_name.clone(),
        incoming.caller_initials.clone(),
        incoming.caller_avatar_source.clone(),
    );
    let local = participant(
        "You".into(),
        my_display_name.clone(),
        initials_from_name(&my_display_name).into(),
        w.get_user_avatar_source(),
    );
    w.set_active_call_data(ActiveCallData {
        call_id: incoming.call_id.clone(),
        channel_id: "".into(),
        title: incoming.caller_name.clone(),
        is_video_call: is_video,
        is_anonymous_call: incoming.is_anonymous_call,
        is_encrypted: true,
        is_identity_verified: false,
        identity_warning: "".into(),
        participant_count: 2,
        duration_seconds: 0,
        is_connected: false,
        participants: ModelRc::new(VecModel::from(vec![remote, local])),
        // No key agreed yet: show nothing (the plain lock) until the SAS
        // emoji arrive, as the dialling view does.
        verification_key_version: "".into(),
        verification_emoji_1: "".into(),
        verification_emoji_2: "".into(),
        verification_emoji_3: "".into(),
        verification_emoji_4: "".into(),
    });
    w.set_call_is_muted(false);
    w.set_call_is_video_enabled(is_video);
    w.set_call_is_screen_sharing(false);
    w.set_active_call_visible(true);
    open_call_screen(w);
}

/// Answering lands on the call, wherever the user was. The call view is docked
/// in the conversation pane, so from Contacts, the chat list or any modal the
/// user answered into nothing visible. Close the modal and, on a phone, bring
/// the pane up with the call filling it. The return-to-call bar (app.slint)
/// covers every later departure.
fn open_call_screen(w: &MainWindow) {
    crate::bin_ui::features::back::reset_to_chats(&w);
    if w.get_compact_layout() {
        // Answered from the list: the call's end drops back to it (app.slint
        // `changed active-call-visible`). Only ever set here — this runs twice
        // per answer, and the second time the pane is already up.
        if !w.get_mobile_show_chat() {
            w.set_call_raised_pane(true);
        }
        w.set_mobile_show_chat(true);
        w.set_active_call_fullscreen(true);
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
    ui_ctx: &Arc<ChatterClient>,
    shared_frames: chatter_core::calls::SharedVideoFrames,
    room_name: String,
    channel_id: String,
    peer_display_name: String,
    is_video: bool,
    verification_emojis: [String; 4],
    peer_avatar: Option<Vec<u8>>,
) {
    let weak = weak.clone();
    let frames = shared_frames.clone();
    let ui_ctx = ui_ctx.clone();
    let _ = slint::invoke_from_event_loop(move || {
        let Some(w) = weak.upgrade() else { return };
        // If `LiveKitRoomEvent::ParticipantJoined` already landed on the UI
        // thread (it races AnonCallConnected via two independent
        // `invoke_from_event_loop` dispatches), the remote slot already
        // carries the server-assigned identity (e.g. "anon-c1954f…").
        // Re-stamping a placeholder here clobbers it, and subsequent
        // `TrackSubscribed` events can't match the identity back to a
        // participant — the screen-share tile then stays in
        // is_video_enabled=false until the user does pause+unpause.
        //
        // Otherwise the remote tile starts with an EMPTY user_id: the peer's
        // LiveKit identity is derived by the server from the ephemeral key it
        // joins with ("anon-" + sha256(pk)[..8], mailbox handlers.go), so
        // nobody knows it before the join. `ParticipantJoined` adopts the
        // empty-id tile (room_events.rs); the old literal "Anonymous" matched
        // no identity, so a join after this render added a second remote
        // tile and the peer's frames never reached this one.
        let existing = w.get_active_call_data().participants;
        let preserved_remote_user_id: Option<slint::SharedString> = existing
            .iter()
            .find(|p| {
                let uid = p.user_id.as_str();
                !uid.is_empty() && uid != "You"
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
        let remote_initials = initials_from_name(&peer_display_name);
        let remote_avatar_source = peer_avatar
            .as_deref()
            .and_then(avatar_bytes_to_image)
            .unwrap_or_default();
        let remote_participant = CallParticipantData {
            user_id: preserved_remote_user_id.unwrap_or_default(),
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
            // An anonymous peer has no identity to have verified; `true` here
            // would be a false "verified" badge the moment one is drawn.
            is_identity_verified: false,
            identity_warning: "".into(),
            participant_count: 2,
            duration_seconds: 0,
            is_connected: true,
            participants: ModelRc::new(VecModel::from(vec![remote_participant, local_participant])),
            verification_key_version: "".into(),
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
        start_call_timer(&w, ui_ctx.clone());
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
            channel_context,
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
            // A channel group call rings with its channel ("in #team"); the
            // modal then offers it as a group call to join.
            let channel_name = match kind {
                OrchestratedCallKind::GroupChannel => channel_context.unwrap_or_default(),
                _ => String::new(),
            };
            // Caller avatar — core owns the "server-DM profile vs anon-DM row"
            // source rule. Pass bytes (Send) and decode on the UI thread;
            // slint::Image is not Send.
            let avatar_bytes = ui_ctx.resolve_call_participant(&peer_id_str).avatar_bytes;

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
                        avatar_bytes.as_deref(),
                    );
                    if !channel_name.is_empty() {
                        let mut data = w.get_incoming_call_data();
                        data.channel_name = channel_name.into();
                        w.set_incoming_call_data(data);
                    }
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
            // A local hangup already took the view down. Anything else ended
            // the call under the user, and says so — a call screen that just
            // vanishes reads as a crash.
            let notice = match reason {
                CallEndReason::LocalHangup => return,
                // The accepted call is about to be painted over this one.
                CallEndReason::ReplacedByIncoming => None,
                // Only ever a withdrawal of a ringing call, never an active one.
                CallEndReason::AnsweredElsewhere => None,
                CallEndReason::RemoteHangup => Some(("Call ended", "info")),
                CallEndReason::Declined => Some(("Call declined", "info")),
                CallEndReason::NoAnswer => Some(("No answer", "info")),
                CallEndReason::Error(_) => Some((super::outgoing::CALL_FAILED, "error")),
            };
            let weak = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                let Some(w) = weak.upgrade() else { return };
                w.set_active_call_visible(false);
                super::super::auth::initial_data::hide_incoming_call(&w);
                if let Some((message, variant)) = notice {
                    stop_call_timer();
                    stop_video_frame_timer();
                    w.set_toast_message(message.into());
                    w.set_toast_variant(variant.into());
                    w.set_toast_visible(true);
                }
            });
        }

        CallOrchestratorEvent::OutgoingStarted { call, .. }
        // Too late to ring; core already filed it as a missed call.
        | CallOrchestratorEvent::IncomingMissed { call, .. } => {
            tracing::debug!(call = %call.as_str(), "Orchestrator state transition");
        }

        CallOrchestratorEvent::ActiveConnected { call, .. } => {
            let weak = weak.clone();
            let ui_ctx = ui_ctx.clone();
            let _ = slint::invoke_from_event_loop(move || {
                let Some(w) = weak.upgrade() else { return };
                let mut data = w.get_active_call_data();
                if data.call_id.as_str() != call.as_str() {
                    return;
                }
                data.is_connected = true;
                w.set_active_call_data(data);
                start_call_timer(&w, ui_ctx);
            });
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
                info.peer_avatar,
            );
        }
    }
}

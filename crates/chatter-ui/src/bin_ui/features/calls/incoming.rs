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
use crate::bin_ui::converters::initials_from_name;
use crate::{ActiveCallData, CallParticipantData, MainWindow};
use chatter_core::client::ChatterClient;
use chatter_core::client::calls::AcceptedIncomingCall;
use chatter_core::services::call_worker::{
    CallEndReason, CallOrchestratorEvent, OrchestratedCallKind,
};

use super::timers::{
    start_call_timer, start_video_frame_timer, stop_call_timer, stop_video_frame_timer,
};

thread_local! {
    // Only a user's tap dismisses an identity. Unrelated async rendering is
    // never inferred to be dismissal merely because the modal is hidden.
    static DISMISSED_INCOMING: std::cell::RefCell<std::collections::HashSet<(u64, String)>> = Default::default();
    #[cfg(target_os = "android")]
    static PERMISSION_PROMPTED: std::cell::RefCell<std::collections::HashSet<(u64, String)>> = Default::default();
}

fn dismiss_incoming(revision: u64, call: &str) {
    DISMISSED_INCOMING.with(|dismissed| {
        let mut dismissed = dismissed.borrow_mut();
        dismissed.retain(|(owner, _)| *owner == revision);
        dismissed.insert((revision, call.to_owned()));
    });
}

/// Wire incoming call accept/decline callbacks.
pub fn wire_incoming_call(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak = window.as_weak();
    let ctx_for_accept = ui_ctx.clone();
    let ctx_for_decline = ui_ctx.clone();

    window.global::<crate::CallView>().on_accept_incoming_call({
        let weak = weak.clone();
        move |call_id| {
            tracing::info!(target: "call_setup::accept_ui_tapped", "Call setup diagnostic");
            let call_id_str = call_id.to_string();
            let weak = weak.clone();
            let ui_ctx = ctx_for_accept.clone();
            let revision = ui_ctx.auth_snapshot().session_revision;
            dismiss_incoming(revision, &call_id_str);

            let replaces_active = weak
                .upgrade()
                .map(|w| {
                    w.global::<crate::CallView>()
                        .get_incoming_call_replaces_active()
                })
                .unwrap_or(false);
            #[cfg(target_os = "android")]
            let is_video = weak
                .upgrade()
                .map(|w| {
                    w.global::<crate::CallView>()
                        .get_incoming_call_data()
                        .is_video_call
                })
                .unwrap_or(false);

            // Swap the modal for the call view now; the accept flow repaints
            // it with the real call on success.
            if let Some(w) = weak.upgrade() {
                show_answering(&w);
                w.global::<crate::CallView>()
                    .set_incoming_call_visible(false);
                w.global::<crate::CallView>()
                    .set_incoming_call_replaces_active(false);
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
                    if let Some((message, name)) = denied_call_permission(is_video) {
                        if ui_ctx.call_session_is_current(revision) {
                            fail_incoming_call(&weak, &ui_ctx, revision, &call_id_str, message);
                            permissions::show_call_permission_denied(name);
                        }
                        tokio::spawn(async move {
                            let _ = ui_ctx
                                .decline_presented_incoming_call_in_session(&call_id_str, revision)
                                .await;
                        });
                        return;
                    }
                }
                tracing::info!(target: "call_setup::accept_permissions_ready", "Call setup diagnostic");
                tokio::spawn(accept(weak, ui_ctx, call_id_str, replaces_active, revision));
            })
            .unwrap();
        }
    });

    window
        .global::<crate::CallView>()
        .on_decline_incoming_call({
            let weak = weak.clone();
            move |call_id| {
                let call_id_str = call_id.to_string();
                let ui_ctx = ctx_for_decline.clone();
                let revision = ui_ctx.auth_snapshot().session_revision;
                dismiss_incoming(revision, &call_id_str);

                if let Some(w) = weak.upgrade() {
                    w.global::<crate::CallView>()
                        .set_incoming_call_visible(false);
                    w.global::<crate::CallView>()
                        .set_incoming_call_replaces_active(false);
                }

                tokio::spawn(async move {
                    if let Err(e) = ui_ctx
                        .decline_presented_incoming_call_in_session(&call_id_str, revision)
                        .await
                    {
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
    revision: u64,
) {
    if !ui_ctx.call_session_is_current(revision) {
        return;
    }
    match ui_ctx
        .accept_presented_incoming_call_in_session(&call_id, replaces_active, revision)
        .await
    {
        Ok(accepted) => render_accepted_call(&weak, &ui_ctx, accepted, revision),
        Err(e) => {
            tracing::error!(error = %e, "Failed to accept incoming call");
            // The answering view went up on the tap; take it down and say why.
            fail_incoming_call(
                &weak,
                &ui_ctx,
                revision,
                &call_id,
                if matches!(e, chatter_core::client::ClientError::CallEnded) {
                    "Call ended"
                } else {
                    super::outgoing::CALL_FAILED
                },
            );
        }
    }
}

#[cfg(target_os = "android")]
fn denied_call_permission(is_video: bool) -> Option<(&'static str, &'static str)> {
    use crate::platform::android::permissions;
    if !permissions::has(permissions::RECORD_AUDIO) {
        Some(("Microphone permission is needed for calls", "Microphone"))
    } else if is_video && !permissions::has(permissions::CAMERA) {
        Some(("Camera permission is needed for video calls", "Camera"))
    } else {
        None
    }
}

fn fail_incoming_call(
    weak: &slint::Weak<MainWindow>,
    ui_ctx: &Arc<ChatterClient>,
    revision: u64,
    call_id: &str,
    message: &'static str,
) {
    let weak = weak.clone();
    let ui_ctx = ui_ctx.clone();
    let expected = call_id.to_owned();
    let _ = slint::invoke_from_event_loop(move || {
        if !ui_ctx.call_session_is_current(revision) {
            return;
        }
        let Some(w) = weak.upgrade() else { return };
        if w.global::<crate::CallView>()
            .get_active_call_data()
            .call_id
            .as_str()
            != expected
        {
            return;
        }
        super::outgoing::fail_call(&weak, message);
    });
}

/// Paint the Slint active-call view from the client's accept result.
fn render_accepted_call(
    weak: &slint::Weak<MainWindow>,
    ui_ctx: &Arc<ChatterClient>,
    accepted: AcceptedIncomingCall,
    revision: u64,
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
                if !ui_ctx.call_session_is_current(revision) {
                    return;
                }
                if w.global::<crate::CallView>()
                    .get_active_call_data()
                    .call_id
                    .as_str()
                    != setup.call_id.to_string()
                {
                    return;
                }
                let Some(worker) = ui_ctx.call_service().get() else {
                    return;
                };
                let identity = setup.call_id.to_string();
                worker.with_current_active_presentation(revision, &identity, || {
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
                        avatar_source: w.global::<crate::SidebarPane>().get_user_avatar_source(),
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
                    w.global::<crate::CallView>()
                        .set_active_call_data(call_data);
                    w.global::<crate::CallView>().set_call_is_muted(false);
                    w.global::<crate::CallView>()
                        .set_call_is_video_enabled(is_video);
                    w.global::<crate::CallView>()
                        .set_call_is_screen_sharing(false);
                    w.global::<crate::CallView>().set_active_call_visible(true);
                    start_call_timer(&w, ui_ctx.clone());
                    start_video_frame_timer(&w, frames);
                    open_call_screen(&w);
                });
            });
            // Media is auto-published by the call worker after connect.
        }

        AcceptedIncomingCall::Group { setup, title } => {
            let weak = weak.clone();
            let frames = shared_frames.clone();
            let ui_ctx = ui_ctx.clone();
            let _ = slint::invoke_from_event_loop(move || {
                let Some(w) = weak.upgrade() else { return };
                if !ui_ctx.call_session_is_current(revision) {
                    return;
                }
                if w.global::<crate::CallView>()
                    .get_active_call_data()
                    .call_id
                    .as_str()
                    != setup.call_id.to_string()
                {
                    return;
                }
                let Some(worker) = ui_ctx.call_service().get() else {
                    return;
                };
                let identity = setup.call_id.to_string();
                worker.with_current_active_presentation(revision, &identity, || {
                    // Keep the remote tiles already up: the answering view's
                    // caller tile, and any `ParticipantJoined` for the people
                    // already in the room that landed before this repaint. Each
                    // later joiner adds its own tile (room_events.rs).
                    let existing = w
                        .global::<crate::CallView>()
                        .get_active_call_data()
                        .participants;
                    let mut participants: Vec<CallParticipantData> = existing.iter().collect();
                    participants.pop(); // the local tile, pinned last; rebuilt below
                    participants.push(CallParticipantData {
                        user_id: current_user_id.clone().into(),
                        display_name: my_display_name.clone().into(),
                        initials: initials_from_name(&my_display_name).into(),
                        avatar_source: w.global::<crate::SidebarPane>().get_user_avatar_source(),
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
                    w.global::<crate::CallView>()
                        .set_active_call_data(call_data);
                    w.global::<crate::CallView>().set_call_is_muted(false);
                    w.global::<crate::CallView>()
                        .set_call_is_video_enabled(setup.is_video);
                    w.global::<crate::CallView>()
                        .set_call_is_screen_sharing(false);
                    w.global::<crate::CallView>().set_active_call_visible(true);
                    start_call_timer(&w, ui_ctx.clone());
                    start_video_frame_timer(&w, frames);
                    open_call_screen(&w);
                });
            });
        }
    }
}

/// Put the call view up the moment Accept is tapped, from the modal's own
/// data. Answering waits on the mic prompt, the answer send and the LiveKit
/// join — seconds on a good network, the 10 s connect timeout on a bad one —
/// and until now all of it rendered as the chat list, which read as the call
/// having been dropped. `render_accepted_call` overwrites this with the real
/// call; a failure takes it down (`fail_call`).
fn show_answering(w: &MainWindow) {
    let incoming = w.global::<crate::CallView>().get_incoming_call_data();
    let is_video = incoming.is_video_call;
    let my_display_name = w.global::<crate::SidebarPane>().get_user_display_name();
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
    let remote = participant(
        incoming.caller_id.clone(),
        incoming.caller_name.clone(),
        incoming.caller_initials.clone(),
        incoming.caller_avatar_source.clone(),
    );
    let local = participant(
        "You".into(),
        my_display_name.clone(),
        initials_from_name(&my_display_name).into(),
        w.global::<crate::SidebarPane>().get_user_avatar_source(),
    );
    w.global::<crate::CallView>()
        .set_active_call_data(ActiveCallData {
            call_id: incoming.call_id.clone(),
            channel_id: "".into(),
            title: incoming.caller_name.clone(),
            is_video_call: is_video,
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
    w.global::<crate::CallView>().set_call_is_muted(false);
    w.global::<crate::CallView>()
        .set_call_is_video_enabled(is_video);
    w.global::<crate::CallView>()
        .set_call_is_screen_sharing(false);
    w.global::<crate::CallView>().set_active_call_visible(true);
    open_call_screen(w);
}

/// Answering lands on the call, wherever the user was. The call view is docked
/// in the conversation pane, so from Contacts, the chat list or any modal the
/// user answered into nothing visible. Close the modal and, on a phone, bring
/// the pane up with the call filling it. The return-to-call bar (app.slint)
/// covers every later departure.
fn open_call_screen(w: &MainWindow) {
    let call_view = w.global::<crate::CallView>();
    call_view.invoke_save_call_route();
    crate::bin_ui::features::back::reset_to_chats(&w);
    if w.get_compact_layout() {
        // Answered from the list: the call's end drops back to it (app.slint
        // `changed active-call-visible`). Only ever set here — this runs twice
        // per answer, and the second time the pane is already up.
        if !w.global::<crate::SidebarPane>().get_mobile_show_chat() {
            w.global::<crate::CallView>().set_call_raised_pane(true);
        }
        w.global::<crate::SidebarPane>().set_mobile_show_chat(true);
        w.global::<crate::CallView>()
            .set_active_call_fullscreen(true);
    }
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

        let Some(state) = ui_ctx.incoming_call_presentation_state() else { return };
        let mut changes = state.subscribe_changes();
        let mut auth_events = ui_ctx.subscribe_auth_snapshots();
        let mut revision = ui_ctx.auth_snapshot().session_revision;
        changes.borrow_and_update();
        reconcile_incoming_modal(&weak, &ui_ctx, &state, revision);
        loop {
            tokio::select! {
                biased;
                auth = auth_events.recv() => {
                    if matches!(auth, Err(tokio::sync::broadcast::error::RecvError::Closed)) { break; }
                    let next = ui_ctx.auth_snapshot().session_revision;
                    if next != revision {
                        revision = next;
                        rx = rx.resubscribe();
                        let reset_ctx = ui_ctx.clone(); let reset_weak = weak.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            if reset_ctx.auth_snapshot().session_revision != next { return; }
                            if let Some(w) = reset_weak.upgrade() {
                                super::super::auth::initial_data::hide_incoming_call(&w);
                                w.global::<crate::CallView>().set_incoming_call_replaces_active(false);
                                w.global::<crate::CallView>().set_active_call_visible(false);
                            }
                        });
                    }
                    reconcile_incoming_modal(&weak, &ui_ctx, &state, revision);
                }
                changed = changes.changed() => {
                    if changed.is_err() { break; }
                    changes.borrow_and_update();
                    reconcile_incoming_modal(&weak, &ui_ctx, &state, revision);
                }
                event = rx.recv() => match event {
                    Ok(CallOrchestratorEvent::IncomingPresented { .. }) => {
                        reconcile_incoming_modal(&weak, &ui_ctx, &state, revision);
                    }
                    Ok(CallOrchestratorEvent::IncomingWithdrawn { call, reason }) => {
                        reconcile_incoming_modal(&weak, &ui_ctx, &state, revision);
                        if matches!(reason, CallEndReason::NoAnswer | CallEndReason::RemoteHangup) {
                            let was_answering = DISMISSED_INCOMING.with(|dismissed| {
                                dismissed.borrow().contains(&(revision, call.as_str().to_owned()))
                            });
                            #[cfg(target_os = "android")]
                            let was_prompted = PERMISSION_PROMPTED.with(|prompted| {
                                prompted.borrow().contains(&(revision, call.as_str().to_owned()))
                            });
                            #[cfg(not(target_os = "android"))]
                            let was_prompted = false;
                            if was_answering || was_prompted {
                                let weak = weak.clone();
                                let ui_ctx = ui_ctx.clone();
                                let id = call.as_str().to_owned();
                                let _ = slint::invoke_from_event_loop(move || {
                                    if !ui_ctx.call_session_is_current(revision) { return; }
                                    let Some(w) = weak.upgrade() else { return };
                                    if w.global::<crate::CallView>().get_active_call_data().call_id.as_str() == id {
                                        w.global::<crate::CallView>().set_active_call_visible(false);
                                    }
                                    w.set_toast_message(if was_answering { "Call ended" } else { "Missed call" }.into());
                                    w.set_toast_variant("info".into());
                                    w.set_toast_visible(true);
                                });
                            }
                        }
                    }
                    Ok(event) => {
                        if ui_ctx.call_session_is_current(revision) && state.session_revision() == Some(revision) {
                            handle_orchestrator_event(&weak, &ui_ctx, event, revision);
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => reconcile_incoming_modal(&weak, &ui_ctx, &state, revision),
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    })
    .expect("spawn call orchestrator subscriber");
}

/// Read authority at UI execution time, rather than capturing a snapshot
/// before the event-loop queue. Buffered events/queued closures cannot bring
/// back withdrawn or accepted calls, and recovery never changes the active UI.
fn reconcile_incoming_modal(
    weak: &slint::Weak<MainWindow>,
    ui_ctx: &Arc<ChatterClient>,
    state: &chatter_core::services::call_worker::IncomingCallPresentationState,
    revision: u64,
) {
    let weak = weak.clone();
    let ui_ctx = ui_ctx.clone();
    let state = state.clone();
    let _ = slint::invoke_from_event_loop(move || {
        let Some(w) = weak.upgrade() else { return };
        if !ui_ctx.call_session_is_current(revision) {
            return;
        }
        state.with_snapshot(|snapshot| {
            if snapshot.session_revision != Some(revision) {
                return;
            }
            let Some(CallOrchestratorEvent::IncomingPresented {
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
            }) = snapshot.presentations.last()
            else {
                super::super::auth::initial_data::hide_incoming_call(&w);
                w.global::<crate::CallView>()
                    .set_incoming_call_replaces_active(false);
                return;
            };
            if DISMISSED_INCOMING.with(|dismissed| {
                dismissed
                    .borrow()
                    .contains(&(revision, call.as_str().to_owned()))
            }) {
                return;
            }
            if w.global::<crate::CallView>().get_incoming_call_visible()
                && w.global::<crate::CallView>()
                    .get_incoming_call_data()
                    .call_id
                    .as_str()
                    == call.as_str()
            {
                w.global::<crate::CallView>()
                    .set_incoming_call_replaces_active(*replaces_active);
                return;
            }
            let peer_id_str = peer_id.to_string();
            let avatar_bytes = ui_ctx.resolve_call_participant(&peer_id_str).avatar_bytes;
            super::super::auth::initial_data::show_incoming_call(
                &w,
                call.as_str(),
                &peer_id_str,
                peer_name,
                if *is_video { "video" } else { "voice" },
                *is_identity_verified,
                identity_warning,
                avatar_bytes.as_deref(),
            );
            if *kind == OrchestratedCallKind::GroupChannel {
                let mut data = w.global::<crate::CallView>().get_incoming_call_data();
                data.channel_name = channel_context.clone().unwrap_or_default().into();
                w.global::<crate::CallView>().set_incoming_call_data(data);
            }
            w.global::<crate::CallView>()
                .set_incoming_call_replaces_active(*replaces_active);
            #[cfg(target_os = "android")]
            prompt_incoming_permissions(&w, &ui_ctx, revision, call.as_str(), *is_video);
        });
    });
}

#[cfg(target_os = "android")]
fn prompt_incoming_permissions(
    w: &MainWindow,
    ui_ctx: &Arc<ChatterClient>,
    revision: u64,
    call_id: &str,
    is_video: bool,
) {
    use crate::platform::android::permissions;
    if denied_call_permission(is_video).is_none() {
        return;
    }
    let key = (revision, call_id.to_owned());
    if !PERMISSION_PROMPTED.with(|prompted| prompted.borrow_mut().insert(key)) {
        return;
    }
    let weak = w.as_weak();
    let ui_ctx = ui_ctx.clone();
    let call_id = call_id.to_owned();
    let _ = slint::spawn_local(async move {
        let mut requested = vec![permissions::RECORD_AUDIO];
        if is_video {
            requested.push(permissions::CAMERA);
        }
        permissions::ensure(&requested).await;
        let Some((message, name)) = denied_call_permission(is_video) else {
            return;
        };
        if !ui_ctx.call_session_is_current(revision) {
            return;
        }
        let Some(w) = weak.upgrade() else { return };
        // Accept owns the same permission result when the user tapped while
        // the dialog was open. It will decline and explain that decision.
        if w.global::<crate::CallView>().get_active_call_visible()
            && w.global::<crate::CallView>()
                .get_active_call_data()
                .call_id
                .as_str()
                == call_id
        {
            return;
        }
        let still_ringing = ui_ctx.incoming_call_presentation_state().is_some_and(|state| {
            state.with_snapshot(|snapshot| snapshot.session_revision == Some(revision)
                && snapshot.presentations.iter().any(|event| matches!(event,
                    CallOrchestratorEvent::IncomingPresented { call, .. } if call.as_str() == call_id)))
        });
        if !still_ringing {
            return;
        }
        dismiss_incoming(revision, &call_id);
        w.global::<crate::CallView>()
            .set_incoming_call_visible(false);
        w.global::<crate::CallView>()
            .set_incoming_call_replaces_active(false);
        w.set_toast_message(message.into());
        w.set_toast_variant("error".into());
        w.set_toast_visible(true);
        permissions::show_call_permission_denied(name);
        tokio::spawn(async move {
            let _ = ui_ctx
                .decline_presented_incoming_call_in_session(&call_id, revision)
                .await;
        });
    });
}

fn handle_orchestrator_event(
    weak: &slint::Weak<MainWindow>,
    ui_ctx: &Arc<ChatterClient>,
    event: CallOrchestratorEvent,
    revision: u64,
) {
    match event {
        // Presentation payloads are hints only; subscriber reconciles authority.
        CallOrchestratorEvent::IncomingPresented { .. }
        | CallOrchestratorEvent::IncomingWithdrawn { .. } => {}

        CallOrchestratorEvent::CameraUnavailable { call } => {
            let weak = weak.clone();
            let ui_ctx = ui_ctx.clone();
            let _ = slint::spawn_local(async move {
                // Initial auto-publish can fail before the accept path paints
                // its active-call view. Apply the failure once that view exists.
                for _ in 0..30 {
                    let Some(w) = weak.upgrade() else { return };
                    if !ui_ctx.call_session_is_current(revision) { return; }
                    if w.global::<crate::CallView>().get_active_call_data().call_id.as_str() == call.as_str() {
                        w.global::<crate::CallView>().set_call_is_video_enabled(false);
                        if let Some(local_id) = ui_ctx.session_current_user_id() {
                            let data = w.global::<crate::CallView>().get_active_call_data();
                            for index in 0..data.participants.row_count() {
                                if let Some(mut participant) = data.participants.row_data(index) {
                                    if participant.user_id.as_str() == local_id.to_string() {
                                        participant.is_video_enabled = false;
                                        if let Some(model) = data.participants.as_any().downcast_ref::<VecModel<CallParticipantData>>() {
                                            model.set_row_data(index, participant);
                                        }
                                        break;
                                    }
                                }
                            }
                        }
                        w.set_toast_message("Camera unavailable".into());
                        w.set_toast_variant("error".into());
                        w.set_toast_visible(true);
                        return;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
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
                // An established session the SDK and one fresh-token rejoin
                // could not bring back is a loss, not a failed connect.
                CallEndReason::Error(ref message)
                    if message == chatter_core::services::call_worker::CONNECTION_LOST =>
                {
                    Some(("Connection lost", "error"))
                }
                CallEndReason::Error(_) => Some((super::outgoing::CALL_FAILED, "error")),
            };
            let weak = weak.clone();
            let ui_ctx = ui_ctx.clone();
            let _ = slint::invoke_from_event_loop(move || {
                let Some(w) = weak.upgrade() else { return };
                if !ui_ctx.call_session_is_current(revision) { return; }
                if w.global::<crate::CallView>().get_active_call_data().call_id.as_str() != call.as_str() { return; }
                w.global::<crate::CallView>().set_active_call_visible(false);
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
                if !w.global::<crate::CallView>().get_active_call_visible()
                    || !ui_ctx.call_session_is_current(revision)
                    || ui_ctx.call_start_was_cancelled(call.as_str())
                { return; }
                let Some(worker) = ui_ctx.call_service().get() else { return };
                worker.with_current_active_presentation(revision, call.as_str(), || {
                let mut data = w.global::<crate::CallView>().get_active_call_data();
                if data.call_id.as_str() != call.as_str() {
                    return;
                }
                data.is_connected = true;
                w.global::<crate::CallView>().set_active_call_data(data);
                start_call_timer(&w, ui_ctx.clone());
                });
            });
        }
    }
}

#[cfg(test)]
mod session_tests {
    use super::*;
    use chatter_core::domain::ids::UserId;
    use chatter_core::services::call_worker::{CallHandle, CallWorkerHandle, OrchestratedCallKind};

    const CHILD_MARKER: &str = "CHATTER_UI_INCOMING_SESSION_TEST_CHILD";
    const FIXTURE_COMPLETED: &str = "incoming session fixture completed";

    fn run_isolated_session_test() {
        use std::io::{Read, Seek, SeekFrom};
        use std::process::{Command, Stdio};
        use std::time::{Duration, Instant};

        // Slint's integration backend may only be initialized once per process.
        // Re-run this exact test alone, preserving the inherited no-keyring env.
        let test_name = format!(
            "{}::queued_projection_and_accept_completion_obey_session_and_active_identity",
            module_path!().split_once("::").unwrap().1
        );
        // Files avoid pipe-buffer deadlocks even if the fixture logs heavily.
        let mut stdout = tempfile::tempfile().expect("create child stdout capture");
        let mut stderr = tempfile::tempfile().expect("create child stderr capture");
        let mut child = Command::new(std::env::current_exe().expect("locate UI test executable"))
            .args(["--exact", &test_name, "--nocapture", "--test-threads=1"])
            .env(CHILD_MARKER, &test_name)
            .stdin(Stdio::null())
            .stdout(Stdio::from(stdout.try_clone().unwrap()))
            .stderr(Stdio::from(stderr.try_clone().unwrap()))
            .spawn()
            .expect("spawn isolated incoming session test");
        let deadline = Instant::now() + Duration::from_secs(120);
        let result = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Ok(status),
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                Ok(None) => break Err("child exceeded 120-second timeout".to_owned()),
                Err(error) => break Err(format!("polling child failed: {error}")),
            }
        };
        // Never signal another test or service; reap only the child we spawned.
        let cleanup = if result.is_err() {
            let killed = child.kill();
            let reaped = child.wait();
            format!("; child kill: {killed:?}; child wait: {reaped:?}")
        } else {
            String::new()
        };
        let mut stdout_text = String::new();
        let mut stderr_text = String::new();
        stdout.seek(SeekFrom::Start(0)).unwrap();
        stderr.seek(SeekFrom::Start(0)).unwrap();
        stdout.read_to_string(&mut stdout_text).unwrap();
        stderr.read_to_string(&mut stderr_text).unwrap();
        assert!(
            matches!(&result, Ok(status) if status.success())
                && stderr_text.lines().any(|line| line == FIXTURE_COMPLETED),
            "isolated test {test_name}: {result:?}{cleanup}\nstdout:\n{stdout_text}\nstderr:\n{stderr_text}"
        );
    }

    fn pump(window: &MainWindow) {
        slint::Timer::single_shot(std::time::Duration::from_millis(50), || {
            slint::quit_event_loop().unwrap()
        });
        window.show().unwrap();
        slint::run_event_loop().unwrap();
    }

    #[test]
    fn queued_projection_and_accept_completion_obey_session_and_active_identity() {
        let test_name = format!(
            "{}::queued_projection_and_accept_completion_obey_session_and_active_identity",
            module_path!().split_once("::").unwrap().1
        );
        if std::env::var(CHILD_MARKER).ok().as_deref() != Some(test_name.as_str()) {
            run_isolated_session_test();
            return;
        }
        i_slint_backend_testing::init_integration_test_with_system_time();
        let runtime = super::super::timers::tests::runtime();
        let _entered = runtime.enter();
        let dir = tempfile::tempdir().unwrap();
        let db = chatter_core::system::Directories::with_base_dir(dir.path().to_owned())
            .unwrap()
            .database_path();
        std::fs::write(
            db.parent()
                .unwrap()
                .join(chatter_core::system::keyring::NO_KEYRING_MARKER),
            b"",
        )
        .unwrap();
        let client = ChatterClient::builder()
            .data_dir(dir.path().to_string_lossy())
            .build()
            .unwrap();
        let pool = chatter_core::network::grpc::GrpcClientPool::new("https://localhost:1").unwrap();
        let worker = CallWorkerHandle::spawn(pool.call_client());
        assert!(client.call_service().set(worker.clone()).is_ok());
        let window = MainWindow::new().unwrap();
        let alice = UserId::new();
        client.testing_set_logged_in(alice, 1, "Alice".into());
        let old = client.auth_snapshot().session_revision;
        let state = worker.incoming_presentation_state();
        state.begin_session(old);
        reconcile_incoming_modal(&window.as_weak(), &client, &state, old);
        client.testing_invalidate_session();
        runtime
            .block_on(worker.reset_presentation_session(old))
            .unwrap();
        client.testing_set_logged_in(UserId::new(), 1, "Bob".into());
        client.testing_set_logged_in(alice, 1, "Alice again".into());
        let revision = client.auth_snapshot().session_revision;
        state.begin_session(revision);
        let current = worker.for_presentation_session(revision);
        current.orch_present_incoming(
            CallHandle::from("B"),
            OrchestratedCallKind::GroupChannel,
            alice,
            "Current ring".into(),
            false,
            false,
            String::new(),
            None,
            None,
            String::new(),
        );
        runtime
            .block_on(current.incoming_presentation_snapshot())
            .unwrap();
        pump(&window);
        assert!(
            !window
                .global::<crate::CallView>()
                .get_incoming_call_visible(),
            "old queued owner cannot render the new ring"
        );
        reconcile_incoming_modal(&window.as_weak(), &client, &state, revision);
        pump(&window);
        assert!(
            window
                .global::<crate::CallView>()
                .get_incoming_call_visible()
        );
        assert_eq!(
            window
                .global::<crate::CallView>()
                .get_incoming_call_data()
                .call_id
                .as_str(),
            "B"
        );
        // Both accept branches must consult actor lifecycle,
        // even when the hidden UI retains the accepted call's exact identity.
        for group in [false, true] {
            let id = chatter_core::domain::ids::CallId::new();
            let channel = chatter_core::domain::ids::ChannelId::new();
            let kind = if group {
                OrchestratedCallKind::GroupChannel
            } else {
                OrchestratedCallKind::DmStandard
            };
            let accepted = || {
                let setup = chatter_core::domain::call::CallSetupResult {
                    call_id: id,
                    channel_id: channel,
                    verification_emojis: Default::default(),
                    is_video: false,
                    started_at_ns: 0,
                };
                if group {
                    AcceptedIncomingCall::Group {
                        setup,
                        title: "Valid group".into(),
                    }
                } else {
                    AcceptedIncomingCall::Dm {
                        setup,
                        peer_id: alice,
                        peer_name: "Valid peer".into(),
                        is_video: false,
                    }
                }
            };
            // This fixture supplies presentation authority, without a media room.
            // The query is a FIFO actor barrier; production completion still requires media match.
            current.orch_mark_connected(CallHandle::from(id.to_string()), kind, alice, false);
            runtime.block_on(current.orch_active_kind());
            let mut data = window.global::<crate::CallView>().get_active_call_data();
            data.call_id = id.to_string().into();
            data.is_connected = false;
            window
                .global::<crate::CallView>()
                .set_active_call_data(data);
            render_accepted_call(&window.as_weak(), &client, accepted(), revision);
            pump(&window);
            assert!(
                window.global::<crate::CallView>().get_active_call_visible(),
                "current accepted identity must render"
            );
            assert!(
                window
                    .global::<crate::CallView>()
                    .get_active_call_data()
                    .is_connected
            );
            assert!(
                window
                    .global::<crate::CallView>()
                    .get_incoming_call_visible(),
                "newer B ring must survive accepted render"
            );
            assert_eq!(
                window
                    .global::<crate::CallView>()
                    .get_incoming_call_data()
                    .call_id
                    .as_str(),
                "B"
            );

            // Hidden End retains this exact identity and session. Actor authority
            // alone must not restart its timer or mark the retained view connected.
            let mut hidden = window.global::<crate::CallView>().get_active_call_data();
            hidden.is_connected = false;
            window
                .global::<crate::CallView>()
                .set_active_call_data(hidden);
            window
                .global::<crate::CallView>()
                .set_active_call_visible(false);
            handle_orchestrator_event(
                &window.as_weak(),
                &client,
                CallOrchestratorEvent::ActiveConnected {
                    call: CallHandle::from(id.to_string()),
                    kind,
                    peer_id: alice,
                    is_video: false,
                },
                revision,
            );
            pump(&window);
            assert!(!window.global::<crate::CallView>().get_active_call_visible());
            assert!(
                !window
                    .global::<crate::CallView>()
                    .get_active_call_data()
                    .is_connected
            );
            window
                .global::<crate::CallView>()
                .set_active_call_visible(true);
            handle_orchestrator_event(
                &window.as_weak(),
                &client,
                CallOrchestratorEvent::ActiveConnected {
                    call: CallHandle::from(id.to_string()),
                    kind,
                    peer_id: alice,
                    is_video: false,
                },
                revision,
            );
            pump(&window);
            assert!(
                window
                    .global::<crate::CallView>()
                    .get_active_call_data()
                    .is_connected
            );

            for replaced in [false, true] {
                // Restore authority for A, then queue its paint before the
                // terminal/replacement actor transition and before UI dispatch.
                current.orch_mark_connected(CallHandle::from(id.to_string()), kind, alice, false);
                runtime.block_on(current.orch_active_kind());
                let mut retained = window.global::<crate::CallView>().get_active_call_data();
                retained.call_id = id.to_string().into();
                retained.is_connected = false;
                window
                    .global::<crate::CallView>()
                    .set_active_call_data(retained);
                window
                    .global::<crate::CallView>()
                    .set_active_call_visible(false);
                window
                    .global::<crate::SidebarPane>()
                    .set_mobile_show_chat(false);
                window
                    .global::<crate::CallView>()
                    .set_active_call_fullscreen(false);
                stop_call_timer();
                stop_video_frame_timer();
                render_accepted_call(&window.as_weak(), &client, accepted(), revision);
                handle_orchestrator_event(
                    &window.as_weak(),
                    &client,
                    CallOrchestratorEvent::ActiveConnected {
                        call: CallHandle::from(id.to_string()),
                        kind,
                        peer_id: alice,
                        is_video: false,
                    },
                    revision,
                );
                if replaced {
                    current.orch_mark_connected(
                        CallHandle::from("replacement-active"),
                        OrchestratedCallKind::DmStandard,
                        alice,
                        false,
                    );
                    runtime.block_on(current.orch_active_kind());
                } else {
                    current.orch_mark_ended(
                        CallHandle::from(id.to_string()),
                        chatter_core::services::call_worker::CallEndReason::RemoteHangup,
                    );
                    runtime.block_on(current.orch_active_kind());
                }
                pump(&window);
                assert!(
                    !window.global::<crate::CallView>().get_active_call_visible(),
                    "stale accepted paint cannot resurrect ended/replaced A"
                );
                assert!(
                    !window
                        .global::<crate::CallView>()
                        .get_active_call_data()
                        .is_connected,
                    "stale connected callback cannot mutate retained A"
                );
                assert!(
                    !super::super::timers::tests::timer_started(),
                    "stale connected callback cannot restart the timer"
                );
                assert!(!window.global::<crate::SidebarPane>().get_mobile_show_chat());
                assert!(
                    !window
                        .global::<crate::CallView>()
                        .get_active_call_fullscreen()
                );
                assert!(
                    window
                        .global::<crate::CallView>()
                        .get_incoming_call_visible()
                );
                assert_eq!(
                    window
                        .global::<crate::CallView>()
                        .get_incoming_call_data()
                        .call_id
                        .as_str(),
                    "B"
                );
            }
        }
        worker.shutdown();
        eprintln!("{FIXTURE_COMPLETED}");
    }
}

//! Outgoing call initiation (UI glue).
//!
//! The core crate owns the orchestration: peer lookup, call-type dispatch,
//! pending-state management. `ChatterClient::start_call` returns an
//! [`OutgoingCallStart`] describing the "Calling…" state to render. This
//! module is now a thin wrapper that translates button clicks into that call
//! and paints the returned struct.

use std::sync::Arc;

use slint::{ComponentHandle, ModelRc, VecModel};

use chatter_core::client::ChatterClient;
use chatter_core::client::calls::OutgoingCallStart;
use chatter_core::domain::call::CallType;
use chatter_core::state::ActiveConversation;
use crate::bin_ui::converters::get_initials;
use crate::{ActiveCallData, CallParticipantData, MainWindow};

use super::timers::{start_call_timer, start_video_frame_timer};

/// Wire start call callbacks.
pub fn wire_start_call(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak = window.as_weak();
    let ctx_chat_state = ui_ctx.chat_state().clone();
    let ctx_ui = ui_ctx.clone();

    window.on_start_voice_call({
        let weak = weak.clone();
        let chat_state = ctx_chat_state.clone();
        let ui_ctx = ctx_ui.clone();
        move || {
            spawn_start(weak.clone(), chat_state.active_conversation(), CallType::Voice, ui_ctx.clone());
        }
    });

    window.on_start_video_call({
        let weak = weak.clone();
        let chat_state = ctx_chat_state.clone();
        let ui_ctx = ctx_ui.clone();
        move || {
            spawn_start(weak.clone(), chat_state.active_conversation(), CallType::Video, ui_ctx.clone());
        }
    });
}

/// Put the call view up *before* anything is awaited.
///
/// `start_call` can take the full 30s budget below — permission prompt, peer
/// lookup, LiveKit join — and until this existed the entire wait was rendered as
/// nothing at all. Tapping the call button in a conversation left the user on
/// the conversation, with no pressed state, no view, and no error if it failed:
/// indistinguishable from a button that does not work, which is what people
/// reasonably concluded before tapping it again.
///
/// The view already knows how to draw this moment — `is-connecting` in
/// `active_call_view.slint` keys off "one participant, no elapsed time", which is
/// exactly the state seeded here, so it says "Calling…" with no new property.
/// `render_calling_placeholder` overwrites all of it a moment later with the real
/// call.
fn show_dialling(weak: &slint::Weak<MainWindow>, call_type: CallType, ui_ctx: &Arc<ChatterClient>) {
    let Some(w) = weak.upgrade() else { return };
    let is_video = matches!(call_type, CallType::Video);
    let display_name = ui_ctx.my_display_name().unwrap_or_else(|| "Anonymous".to_string());
    let initials = get_initials(&display_name);

    let local_participant = CallParticipantData {
        user_id: "".into(),
        display_name: display_name.into(),
        initials: initials.into(),
        avatar_source: w.get_user_avatar_source(),
        is_muted: false,
        is_video_enabled: is_video,
        is_screen_sharing: false,
        is_speaking: false,
        is_audio_muted_locally: false,
        is_video_paused_locally: false,
    };

    w.set_active_call_data(ActiveCallData {
        call_id: "".into(),
        channel_id: "".into(),
        // Where the call is going. The conversation header already says this, and
        // it is the only fact about the call we actually know yet.
        title: w.get_channel_name(),
        is_video_call: is_video,
        is_anonymous_call: false,
        is_encrypted: true,
        // Nothing has been verified yet and nothing may claim otherwise. The
        // hourglass is the same placeholder the real dialling state uses.
        is_identity_verified: false,
        identity_warning: "".into(),
        participant_count: 1,
        duration_seconds: 0,
        participants: ModelRc::new(VecModel::from(vec![local_participant])),
        verification_emoji_1: "\u{23F3}".into(),
        verification_emoji_2: "".into(),
        verification_emoji_3: "".into(),
        verification_emoji_4: "".into(),
    });
    w.set_call_is_muted(false);
    w.set_call_is_video_enabled(is_video);
    w.set_call_is_screen_sharing(false);
    w.set_active_call_visible(true);
}

/// Take the call view back down and say why.
///
/// Every failure path used to be `set_active_call_visible(false)` and a log line.
/// With the view now raised at tap time that would be a flash and nothing else,
/// which is worse than the silence it replaced — so failing closed has to come
/// with the reason.
fn fail_call(weak: &slint::Weak<MainWindow>, reason: &str) {
    let Some(w) = weak.upgrade() else { return };
    w.set_active_call_visible(false);
    w.set_toast_message(reason.into());
    w.set_toast_variant("error".into());
    w.set_toast_visible(true);
    w.global::<crate::Haptics>().invoke_error();
}

fn spawn_start(
    weak: slint::Weak<MainWindow>,
    conversation: ActiveConversation,
    call_type: CallType,
    ui_ctx: Arc<ChatterClient>,
) {
    show_dialling(&weak, call_type, &ui_ctx);
    slint::spawn_local(async move {
        if tokio::time::timeout(
            std::time::Duration::from_secs(30),
            start_call_impl(weak.clone(), call_type, conversation, ui_ctx),
        )
        .await
        .is_err()
        {
            tracing::error!("Call setup timed out");
            fail_call(&weak, "The call could not be connected in time.");
        }
    })
    .unwrap();
}

async fn start_call_impl(
    weak: slint::Weak<MainWindow>,
    call_type: CallType,
    conversation: ActiveConversation,
    ui_ctx: Arc<ChatterClient>,
) {
    // Request mic (+ camera for video) on-demand, here at call start, rather
    // than up front. Already-granted permissions are filtered out (no re-prompt).
    #[cfg(target_os = "android")]
    {
        use crate::platform::android::permissions;
        let mut perms = vec![permissions::RECORD_AUDIO];
        if matches!(call_type, CallType::Video) {
            perms.push(permissions::CAMERA);
        }
        permissions::request(&perms);
    }

    let user_id = match ui_ctx.session_current_user_id() {
        Some(id) => id,
        None => {
            tracing::error!("No user ID available");
            fail_call(&weak, "You are not signed in, so the call was not placed.");
            return;
        }
    };
    let display_name = ui_ctx.my_display_name().unwrap_or_else(|| "Anonymous".to_string());
    let channel_name = weak
        .upgrade()
        .map(|w| w.get_channel_name().to_string())
        .unwrap_or_default();

    // Delegate all orchestration to core — it resolves peer / kind / transport.
    let ctx = ui_ctx.clone();
    let dn = display_name.clone();
    // The `JoinError` is handled rather than unwrapped: this runs inside
    // `slint::spawn_local`, i.e. on the UI thread, so unwrapping it turned any
    // panic in the LiveKit/gRPC start path into a panic on the event loop and
    // took the whole app down — including the mesh, which had nothing to do with
    // the call. A failed call should close the call view, not the process.
    let joined = tokio::spawn(async move { ctx.start_call(conversation, call_type, &dn).await }).await;
    let start = match joined {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => {
            tracing::error!(error = %e, "Failed to start call");
            fail_call(&weak, "The call could not be started.");
            return;
        }
        Err(join_error) => {
            tracing::error!(%join_error, "Call start task did not finish");
            fail_call(&weak, "The call could not be started.");
            return;
        }
    };

    tracing::info!(call_id = %start.call_id, "Outgoing call started via core");
    render_calling_placeholder(&weak, &ui_ctx, start, user_id, display_name, channel_name);
}

/// Paint the "Calling…" placeholder from a core-produced setup struct.
fn render_calling_placeholder(
    weak: &slint::Weak<MainWindow>,
    ui_ctx: &Arc<ChatterClient>,
    setup: OutgoingCallStart,
    user_id: chatter_core::domain::ids::UserId,
    display_name: String,
    channel_name: String,
) {
    let Some(w) = weak.upgrade() else { return };

    let local_initials = get_initials(&display_name);
    let local_participant = CallParticipantData {
        user_id: user_id.to_string().into(),
        display_name: display_name.into(),
        initials: local_initials.into(),
        avatar_source: w.get_user_avatar_source(),
        is_muted: false,
        is_video_enabled: setup.is_video,
        is_screen_sharing: false,
        is_speaking: false,
        is_audio_muted_locally: false,
        is_video_paused_locally: false,
    };

    // Pre-known verification emojis (channel group calls) — show them at dial
    // time. For DM + anon, fall back to a "dialling" placeholder glyph until
    // the answer arrives.
    let (e1, e2, e3, e4) = match &setup.verification_emojis {
        Some(v) => (v[0].clone(), v[1].clone(), v[2].clone(), v[3].clone()),
        None if setup.is_anonymous => ("\u{1F512}".into(), "".into(), "".into(), "".into()),
        None => ("\u{23F3}".into(), "".into(), "".into(), "".into()),
    };

    let title = if setup.title.is_empty() && !channel_name.is_empty() {
        channel_name
    } else if setup.title.is_empty() {
        "Calling...".into()
    } else {
        setup.title.clone()
    };

    let call_data = ActiveCallData {
        call_id: setup.call_id.clone().into(),
        channel_id: setup.channel_id.clone().into(),
        title: title.into(),
        is_video_call: setup.is_video,
        is_anonymous_call: setup.is_anonymous,
        is_encrypted: true,
        is_identity_verified: setup.is_anonymous, // anon = no identity to verify
        identity_warning: "".into(),
        participant_count: 1,
        duration_seconds: 0,
        participants: ModelRc::new(VecModel::from(vec![local_participant])),
        verification_emoji_1: e1.into(),
        verification_emoji_2: e2.into(),
        verification_emoji_3: e3.into(),
        verification_emoji_4: e4.into(),
    };
    w.set_active_call_data(call_data);
    w.set_call_is_muted(false);
    w.set_call_is_video_enabled(setup.is_video);
    w.set_call_is_screen_sharing(false);
    w.set_active_call_visible(true);

    // Channel group calls connect synchronously inside `start_group_call`, so
    // start the timers now. DM + anon calls are still "pending" at this point
    // — their timers are started by the `AnonCallConnected` / DM key-exchange
    // paths when the peer answers.
    if setup.verification_emojis.is_some() {
        start_call_timer(&w, None);
        if let Some(frames) = ui_ctx.call_video_frames() {
            start_video_frame_timer(&w, frames);
        }
    }
}

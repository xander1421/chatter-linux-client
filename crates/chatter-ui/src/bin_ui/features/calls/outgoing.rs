//! Outgoing call initiation (UI glue).
//!
//! The core crate owns the orchestration: peer lookup, call-type dispatch,
//! pending-state management. `ChatterClient::start_call` returns an
//! [`OutgoingCallStart`] describing the "Calling…" state to render. This
//! module is now a thin wrapper that translates button clicks into that call
//! and paints the returned struct.

use std::sync::Arc;

use slint::{ComponentHandle, ModelRc, VecModel};

use crate::bin_ui::converters::avatar_bytes_to_image;
use crate::bin_ui::converters::initials_from_name;
use crate::{ActiveCallData, CallParticipantData, MainWindow};
use chatter_core::client::ChatterClient;
use chatter_core::client::calls::OutgoingCallStart;
use chatter_core::domain::call::CallType;
use chatter_core::state::ActiveConversation;

use super::timers::{start_call_timer, start_video_frame_timer};

thread_local! {
    static START: std::cell::RefCell<(u64, Option<chatter_core::client::calls::CallStartAttempt>)> = const { std::cell::RefCell::new((0, None)) };
}
/// End invalidates the pending start synchronously, before another click can run.
pub(super) fn cancel_pending_start() -> bool {
    START.with(|slot| {
        let mut slot = slot.borrow_mut();
        if let Some(attempt) = slot.1.take() {
            attempt.cancel();
            true
        } else {
            false
        }
    })
}
fn current_start(generation: u64) -> bool {
    START.with(|slot| {
        let slot = slot.borrow();
        slot.0 == generation && slot.1.is_some()
    })
}

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
            spawn_start(
                weak.clone(),
                chat_state.active_conversation(),
                CallType::Voice,
                ui_ctx.clone(),
            );
        }
    });

    window.on_start_video_call({
        let weak = weak.clone();
        let chat_state = ctx_chat_state.clone();
        let ui_ctx = ctx_ui.clone();
        move || {
            spawn_start(
                weak.clone(),
                chat_state.active_conversation(),
                CallType::Video,
                ui_ctx.clone(),
            );
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
/// The explicit connection flag keeps this placeholder in "Calling…".
/// `render_calling_placeholder` overwrites all of it a moment later with the real
/// call.
fn show_dialling(weak: &slint::Weak<MainWindow>, call_type: CallType, ui_ctx: &Arc<ChatterClient>) {
    let Some(w) = weak.upgrade() else { return };
    let is_video = matches!(call_type, CallType::Video);
    let display_name = ui_ctx
        .my_display_name()
        .unwrap_or_else(|| "Anonymous".to_string());
    let initials = initials_from_name(&display_name);

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
        is_encrypted: true,
        // Nothing has been verified yet and nothing may claim otherwise. No
        // emoji until keyed: the view shows its plain lock in their place.
        is_identity_verified: false,
        identity_warning: "".into(),
        participant_count: 1,
        duration_seconds: 0,
        is_connected: false,
        participants: ModelRc::new(VecModel::from(vec![local_participant])),
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
}

pub(super) const CALL_FAILED: &str = "Call failed — couldn't connect";
/// Shown when the microphone prompt is refused; the call cannot work without it.
#[cfg(target_os = "android")]
pub(super) const MIC_DENIED: &str = "Calls need microphone access — allow it to call.";

/// Take the call view back down and say why.
///
/// Every failure path used to be `set_active_call_visible(false)` and a log line.
/// With the view now raised at tap time that would be a flash and nothing else,
/// which is worse than the silence it replaced — so failing closed has to come
/// with the reason.
pub(super) fn fail_call(weak: &slint::Weak<MainWindow>, reason: &str) {
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
    cancel_pending_start();
    let revision = ui_ctx.auth_state().get().session_revision;
    let attempt = chatter_core::client::calls::CallStartAttempt::default();
    let generation = START.with(|slot| {
        let mut slot = slot.borrow_mut();
        slot.0 += 1;
        slot.1 = Some(attempt.clone());
        slot.0
    });
    show_dialling(&weak, call_type, &ui_ctx);
    slint::spawn_local(async move {
        // Mic (+ camera for video) is asked for here, on demand, and the call
        // waits for the answer: firing the prompt and carrying on placed the
        // call with no microphone behind the dialog, so the first tap after
        // "Allow" looked like it did nothing. Outside the timeout below — the
        // user reading a system dialog is not the call failing to connect.
        #[cfg(target_os = "android")]
        {
            use crate::platform::android::permissions;
            let mut perms = vec![permissions::RECORD_AUDIO];
            if matches!(call_type, CallType::Video) {
                perms.push(permissions::CAMERA);
            }
            // Only the microphone is required: a refused camera still leaves a
            // working call, and the video button asks again on demand.
            permissions::ensure(&perms).await;
            if !current_start(generation) {
                return;
            }
            if !permissions::has(permissions::RECORD_AUDIO) {
                fail_call(&weak, MIC_DENIED);
                return;
            }
        }
        if !current_start(generation) {
            return;
        }
        if ui_ctx.auth_state().get().session_revision != revision {
            attempt.cancel();
            fail_call(&weak, "Your session changed. Start the call again.");
            return;
        }
        start_call_impl(
            weak.clone(),
            call_type,
            conversation,
            ui_ctx,
            attempt,
            generation,
        )
        .await;
        START.with(|slot| {
            let mut slot = slot.borrow_mut();
            if slot.0 == generation {
                slot.1 = None;
            }
        });
    })
    .unwrap();
}

async fn start_call_impl(
    weak: slint::Weak<MainWindow>,
    call_type: CallType,
    conversation: ActiveConversation,
    ui_ctx: Arc<ChatterClient>,
    attempt: chatter_core::client::calls::CallStartAttempt,
    generation: u64,
) {
    let user_id = match ui_ctx.session_current_user_id() {
        Some(id) => id,
        None => {
            tracing::error!("No user ID available");
            fail_call(&weak, "You are not signed in, so the call was not placed.");
            return;
        }
    };
    let display_name = ui_ctx
        .my_display_name()
        .unwrap_or_else(|| "Anonymous".to_string());
    let channel_name = weak
        .upgrade()
        .map(|w| w.get_channel_name().to_string())
        .unwrap_or_default();

    // Delegate all orchestration to core — it resolves peer / kind / transport.
    let ctx = ui_ctx.clone();
    // The `JoinError` is handled rather than unwrapped: this runs inside
    // `slint::spawn_local`, i.e. on the UI thread, so unwrapping it turned any
    // panic in the LiveKit/gRPC start path into a panic on the event loop and
    // took the whole app down — including the mesh, which had nothing to do with
    // the call. A failed call should close the call view, not the process.
    let revision = ui_ctx.auth_state().get().session_revision;
    let task_attempt = attempt.clone();
    let mut task = tokio::spawn(async move {
        ctx.start_call_attempt(conversation, call_type, task_attempt)
            .await
    });
    let joined = tokio::select! {
        result = &mut task => result,
        _ = attempt.cancelled() => task.await,
        _ = tokio::time::sleep(std::time::Duration::from_secs(30)) => {
            attempt.cancel();
            if current_start(generation) { fail_call(&weak, CALL_FAILED); }
            // Keep ownership: dropping this JoinHandle would detach setup.
            task.await
        }
    };
    if !current_start(generation)
        || attempt.is_cancelled()
        || ui_ctx.auth_state().get().session_revision != revision
    {
        attempt.cancel();
        if let Ok(Ok(start)) = joined {
            ui_ctx.rollback_call_start(&start).await;
        }
        return;
    }
    let start = match joined {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => {
            tracing::error!(error = %e, "Failed to start call");
            fail_call(&weak, CALL_FAILED);
            return;
        }
        Err(join_error) => {
            tracing::error!(%join_error, "Call start task did not finish");
            fail_call(&weak, CALL_FAILED);
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

    let local_initials = initials_from_name(&display_name);
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

    // Handshake setup may supply SAS; group setup remains empty until the
    // presentation snapshot supplies the installed media key and its epoch.
    let [e1, e2, e3, e4] = setup.verification_emojis.clone().unwrap_or_default();

    // A 1:1 call pre-renders the callee's tile, so ringing shows who is being
    // called rather than the caller. It is a remote placed before the local
    // tile (local is pinned last), which the join handler matches by user_id.
    let mut participants = Vec::with_capacity(2);
    if let Some(peer) = &setup.peer {
        participants.push(CallParticipantData {
            user_id: peer.user_id.clone().into(),
            display_name: peer.info.display_name.clone().into(),
            initials: peer.info.initials.clone().into(),
            avatar_source: peer
                .info
                .avatar_bytes
                .as_deref()
                .and_then(avatar_bytes_to_image)
                .unwrap_or_default(),
            is_muted: false,
            is_video_enabled: false,
            is_screen_sharing: false,
            is_speaking: false,
            is_audio_muted_locally: false,
            is_video_paused_locally: false,
        });
    }
    participants.push(local_participant);

    let title = if setup.title.is_empty() {
        channel_name
    } else {
        setup.title.clone()
    };

    let call_data = ActiveCallData {
        call_id: setup.call_id.clone().into(),
        channel_id: setup.channel_id.clone().into(),
        title: title.into(),
        is_video_call: setup.is_video,
        is_encrypted: true,
        // Nothing is verified when the call starts.
        is_identity_verified: false,
        identity_warning: "".into(),
        // Only we are in the room until the callee joins; the pre-rendered
        // callee tile does not count, which keeps the view in its ringing state.
        participant_count: 1,
        duration_seconds: 0,
        is_connected: false,
        participants: ModelRc::new(VecModel::from(participants)),
        verification_key_version: "".into(),
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

    // SAS emojis are absent for channel group calls too. Start presentation
    // polling for every call; fresh core snapshots decide when it is connected.
    // Pending DM calls stay at zero until their answer is processed.
    start_call_timer(&w, ui_ctx.clone());
    if let Some(frames) = ui_ctx.call_video_frames() {
        start_video_frame_timer(&w, frames);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn call_group_mount_starts_timer_without_verification_emojis() {
        i_slint_backend_testing::init_no_event_loop();
        let runtime = super::super::timers::tests::runtime();
        let _guard = runtime.enter();
        let dir = tempfile::tempdir().unwrap();
        // Keep this fixture off the real keyring even when the launcher omits
        // CHATTER_NO_KEYRING. The marker must exist before client construction.
        let db_path = chatter_core::system::Directories::with_base_dir(dir.path().to_path_buf())
            .unwrap()
            .database_path();
        std::fs::write(
            db_path
                .parent()
                .unwrap()
                .join(chatter_core::system::keyring::NO_KEYRING_MARKER),
            b"",
        )
        .unwrap();
        let client = ChatterClient::builder()
            .data_dir(dir.path().to_string_lossy().to_string())
            .build()
            .unwrap();
        let window = MainWindow::new().unwrap();
        super::super::timers::stop_call_timer();
        assert!(!super::super::timers::tests::timer_started());
        let start = OutgoingCallStart {
            call_id: uuid::Uuid::new_v4().to_string(),
            channel_id: uuid::Uuid::new_v4().to_string(),
            is_video: false,
            verification_emojis: None,
            title: String::new(),
            peer: None,
        };
        render_calling_placeholder(
            &window.as_weak(),
            &client,
            start,
            chatter_core::domain::ids::UserId::new(),
            "Me".into(),
            "Group".into(),
        );
        assert!(window.get_active_call_visible());
        assert!(
            super::super::timers::tests::timer_started(),
            "group calls have no SAS emojis but must still start their timer"
        );
        super::super::timers::stop_call_timer();
    }
}

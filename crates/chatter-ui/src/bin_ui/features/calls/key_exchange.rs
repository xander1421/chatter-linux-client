//! Ephemeral key exchange for call encryption.
//!
//! DM call key exchange state and crypto are fully managed by core
//! (CallSignalingService). This module provides the UI-side handler
//! that delegates to core and updates Slint properties from the result.

use std::sync::Arc;

use crate::MainWindow;
use chatter_core::client::ChatterClient;
use chatter_core::domain::ids::UserId;

use super::timers::{start_call_timer, start_video_frame_timer};

/// Handle receiving a call key answer (peer's ephemeral key).
///
/// Delegates to core's complete_dm_call() which handles:
/// 1. Taking pending call state
/// 2. Verifying call ID match
/// 3. ECDH + identity lookup + HKDF (via Signal actor)
/// 4. Updating LiveKit encryption key
/// 5. Deriving verification emojis
///
/// This function only handles UI updates (Slint properties, timers, media).
/// Must be called on the UI thread.
pub async fn handle_call_key_answer(
    weak_window: &slint::Weak<MainWindow>,
    ui_ctx: &Arc<ChatterClient>,
    peer_ephemeral_pk: [u8; 32],
    call_id: String,
    peer_id: UserId,
) {
    let revision = ui_ctx.auth_state().get().session_revision;
    let attempt = ui_ctx.call_completion_attempt(&call_id);
    tracing::info!(
        call_id = %call_id,
        peer_id = %peer_id,
        "Processing call key answer via core"
    );

    // Delegate all crypto and state management to core (gRPC - run on tokio)
    let result = {
        let ctx = ui_ctx.clone();
        let cid = call_id.clone();
        let pid = peer_id;
        tokio::spawn(async move {
            ctx.complete_dm_call_for_session(peer_ephemeral_pk, &cid, &pid, revision)
                .await
        })
        .await
    };

    let setup = match result {
        Ok(Ok(setup)) => setup,
        Ok(Err(e)) => {
            tracing::error!(call_id = %call_id, error = %e, "Failed to complete DM call");
            return;
        }
        // Handled rather than unwrapped: this runs on the UI thread, where an
        // unwrapped `JoinError` turned a panic in the key-exchange path into a
        // dead app instead of a dead call.
        Err(join_error) => {
            tracing::error!(call_id = %call_id, %join_error, "Call key-answer task did not finish");
            return;
        }
    };

    tracing::info!(
        call_id = %call_id,
        "DM call completed, updating UI"
    );

    apply_dm_completion(
        weak_window,
        ui_ctx,
        revision,
        &call_id,
        attempt.as_ref(),
        setup,
    );
}

fn apply_dm_completion(
    weak_window: &slint::Weak<MainWindow>,
    ui_ctx: &Arc<ChatterClient>,
    revision: u64,
    call_id: &str,
    attempt: Option<&chatter_core::client::calls::CallStartAttempt>,
    setup: chatter_core::domain::call::CallSetupResult,
) {
    // Media is auto-published by the call worker after connect — no UI-side
    // publish needed.

    // Update UI with verification emojis from core result
    let shared_frames = ui_ctx.call_video_frames();
    if let Some(w) = weak_window.upgrade() {
        if !w.get_active_call_visible()
            || !ui_ctx.call_session_is_current(revision)
            || attempt.is_some_and(|a| a.is_cancelled())
            || ui_ctx.call_start_was_cancelled(call_id)
        {
            return;
        }
        let mut call_data = w.get_active_call_data();
        if call_data.call_id.as_str() != call_id || setup.call_id.to_string() != call_id {
            return;
        }
        call_data.verification_emoji_1 = setup.verification_emojis[0].clone().into();
        call_data.verification_emoji_2 = setup.verification_emojis[1].clone().into();
        call_data.verification_emoji_3 = setup.verification_emojis[2].clone().into();
        call_data.verification_emoji_4 = setup.verification_emojis[3].clone().into();
        w.set_active_call_data(call_data);

        start_call_timer(&w, ui_ctx.clone());
        if let Some(frames) = shared_frames {
            start_video_frame_timer(&w, frames);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use slint::ComponentHandle;

    #[test]
    fn named_dm_ui_completion_guards_retained_owner_session_and_displayed_identity() {
        const CHILD: &str = "CHATTER_DM_UI_GUARD_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let marker_dir = tempfile::tempdir().unwrap();
            let marker = marker_dir.path().join("assertions-ran");
            let mut child = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "bin_ui::features::calls::key_exchange::tests::named_dm_ui_completion_guards_retained_owner_session_and_displayed_identity", "--test-threads=1"])
                .env(CHILD, &marker).spawn().unwrap();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(45);
            let result = loop {
                match child.try_wait() {
                    Ok(Some(status)) => break Ok(status),
                    Ok(None) if std::time::Instant::now() < deadline => {
                        std::thread::sleep(std::time::Duration::from_millis(20));
                    }
                    Ok(None) => break Err("child exceeded 45-second timeout".to_owned()),
                    Err(error) => break Err(format!("polling child failed: {error}")),
                }
            };
            let cleanup = if result.is_err() {
                let killed = child.kill();
                let reaped = child.wait();
                format!("; child kill: {killed:?}; child wait: {reaped:?}")
            } else {
                String::new()
            };
            assert!(
                matches!(result, Ok(status) if status.success()),
                "isolated named-DM UI test: {result:?}{cleanup}"
            );
            assert_eq!(std::fs::read(marker).unwrap(), b"passed");
            return;
        }
        i_slint_backend_testing::init_integration_test_with_system_time();
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let _entered = runtime.enter();
        crate::init_rt_handle(runtime.handle().clone());
        let dir = tempfile::tempdir().unwrap();
        let dirs = chatter_core::system::Directories::with_base_dir(dir.path().to_owned()).unwrap();
        std::fs::write(
            dirs.database_path()
                .parent()
                .unwrap()
                .join(chatter_core::system::keyring::NO_KEYRING_MARKER),
            b"",
        )
        .unwrap();
        let client = ChatterClient::builder()
            .data_dir(dir.path().to_string_lossy())
            .build()
            .unwrap();
        client
            .auth_state()
            .set_logged_in(UserId::new(), 1, "Fixture".into());
        let revision = client.auth_state().get().session_revision;
        let w = MainWindow::new().unwrap();
        let id = chatter_core::domain::ids::CallId::new();
        let expected = id.to_string();
        let setup = || chatter_core::domain::call::CallSetupResult {
            call_id: id,
            channel_id: chatter_core::domain::ids::ChannelId::new(),
            verification_emojis: ["A".into(), "B".into(), "C".into(), "D".into()],
            is_video: false,
            started_at_ns: 0,
        };
        let original = crate::ActiveCallData {
            call_id: "replacement".into(),
            verification_emoji_1: "keep".into(),
            duration_seconds: 17,
            ..Default::default()
        };
        w.set_active_call_visible(true);
        w.set_active_call_data(original.clone());
        apply_dm_completion(&w.as_weak(), &client, revision, &expected, None, setup());
        assert_eq!(
            w.get_active_call_data().verification_emoji_1.as_str(),
            "keep"
        );
        assert_eq!(w.get_active_call_data().duration_seconds, 17);
        let mut same = original.clone();
        same.call_id = expected.clone().into();
        w.set_active_call_data(same.clone());
        w.set_active_call_visible(false);
        apply_dm_completion(&w.as_weak(), &client, revision, &expected, None, setup());
        assert_eq!(
            w.get_active_call_data().verification_emoji_1.as_str(),
            "keep"
        );
        assert_eq!(w.get_active_call_data().duration_seconds, 17);
        assert!(!w.get_active_call_visible());
        w.set_active_call_visible(true);
        let attempt = chatter_core::client::calls::CallStartAttempt::default();
        attempt.cancel();
        apply_dm_completion(
            &w.as_weak(),
            &client,
            revision,
            &expected,
            Some(&attempt),
            setup(),
        );
        assert_eq!(
            w.get_active_call_data().verification_emoji_1.as_str(),
            "keep"
        );
        assert_eq!(w.get_active_call_data().duration_seconds, 17);
        client.auth_state().invalidate_session();
        client
            .auth_state()
            .set_logged_in(UserId::new(), 1, "Replacement session".into());
        apply_dm_completion(&w.as_weak(), &client, revision, &expected, None, setup());
        assert_eq!(
            w.get_active_call_data().verification_emoji_1.as_str(),
            "keep"
        );
        assert_eq!(w.get_active_call_data().duration_seconds, 17);
        let current = client.auth_state().get().session_revision;
        assert!(w.get_active_call_visible());
        apply_dm_completion(&w.as_weak(), &client, current, &expected, None, setup());
        assert_eq!(w.get_active_call_data().verification_emoji_1.as_str(), "A");
        assert_eq!(w.get_active_call_data().verification_emoji_4.as_str(), "D");
        std::fs::write(std::env::var_os(CHILD).unwrap(), b"passed").unwrap();
    }
}

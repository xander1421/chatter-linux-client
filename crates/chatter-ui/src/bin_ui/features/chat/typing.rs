//! Typing indicator callback for DM and channel conversations.
//!
//! Signal-style typing indicator implementation with:
//! - Refresh timer (10s) - re-sends STARTED while typing
//! - Pause timer (3s) - sends STOPPED after user stops typing
//! - Timer presence = typing state (no separate boolean)
//! - Clear on message send

use std::cell::RefCell;
use std::sync::Arc;
use std::time::{Duration, Instant};

use slint::Timer;
use slint::TimerMode;

use chatter_core::client::ChatterClient;
use chatter_core::services::conversation::ConversationHandle;
use chatter_core::state::ActiveConversation;

/// Typing refresh interval (Signal: 10 seconds)
const TYPING_REFRESH_MS: u64 = 10_000;

/// Typing pause timeout (Signal: 3 seconds)
const TYPING_PAUSE_MS: u64 = 3_000;

/// Throttle interval for bumpTyping calls (Signal: 300ms)
const TYPING_THROTTLE_MS: u64 = 300;

/// Outgoing typing state for a conversation.
struct OutgoingTypingState {
    /// Timer that re-sends STARTED every 10s (kept alive, fires via Repeated mode)
    _refresh_timer: Timer,
    /// Timer that sends STOPPED after 3s of no input
    pause_timer: Timer,
    /// Last time we called bumpTyping (for throttling)
    last_bump: Instant,
    /// Active conversation (typed, no string serialization)
    conversation: ActiveConversation,
}

thread_local! {
    /// Thread-local outgoing typing state.
    /// Must be thread_local because slint::Timer is not Send.
    static OUTGOING_TYPING: RefCell<Option<OutgoingTypingState>> = const { RefCell::new(None) };
}

/// Clear outgoing typing state (no STOPPED message needed).
/// Called when a message is sent (like Signal's clearTypingTimers).
/// The message itself implies "stopped typing" - receiver clears on message receipt.
pub fn clear_outgoing_typing() {
    OUTGOING_TYPING.with(|state_cell| {
        if let Some(state) = state_cell.borrow_mut().take() {
            tracing::debug!(
                conversation = ?state.conversation,
                "TYPING_SEND: Clearing typing state on message send"
            );
        }
    });
}

/// Wire typing indicator callback for DM and channel conversations.
pub fn wire_typing_changed(window: &crate::MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let ui_ctx = ui_ctx.clone();

    window.on_typing_changed(move |is_typing: bool| {
        // Track user activity for presence updates
        if is_typing {
            let ui_ctx_activity = ui_ctx.clone();
            tokio::spawn(async move {
                crate::bin_ui::features::auth::presence_integration::track_activity(&ui_ctx_activity).await;
            });
        }

        let conversation = ui_ctx.chat_state().active_conversation();
        if matches!(conversation, ActiveConversation::None) {
            return;
        }

        // Skip if typing to self (DM only)
        if let ActiveConversation::MailboxDm { peer_id, .. } | ActiveConversation::ServerDm { peer_id, .. } = &conversation {
            if let Some(user_id) = ui_ctx.auth_state().get().current_user_id {
                if peer_id == &user_id {
                    return;
                }
            }
        }

        let handle = ConversationHandle::new(conversation, ui_ctx.clone());

        if is_typing {
            bump_typing(conversation, handle);
        } else {
            // User explicitly stopped typing — clear state and send STOPPED
            OUTGOING_TYPING.with(|state_cell| {
                let state = state_cell.borrow_mut().take();
                if let Some(state) = state {
                    if state.conversation == conversation {
                        tracing::debug!(
                            conversation = ?conversation,
                            "TYPING_SEND: User stopped typing explicitly"
                        );
                        tokio::spawn(async move {
                            let _ = handle.send_typing(false).await;
                        });
                    }
                }
            });
        }
    });
}

/// Signal-style bumpTyping — called on each keystroke, throttled.
///
/// Uses `ConversationHandle` for polymorphic dispatch, replacing the
/// three separate `bump_typing` / `bump_typing_anon_dm` / `bump_typing_anon_group`
/// implementations that were previously 608 lines combined.
fn bump_typing(conversation: ActiveConversation, handle: ConversationHandle) {
    OUTGOING_TYPING.with(|state_cell| {
        let mut state_opt = state_cell.borrow_mut();

        let needs_new_state = match &*state_opt {
            Some(state) => state.conversation != conversation,
            None => true,
        };

        if needs_new_state {
            // Send STOPPED for old conversation if switching
            if let Some(old_state) = state_opt.take() {
                tracing::debug!(
                    old = ?old_state.conversation,
                    new = ?conversation,
                    "TYPING_SEND: Switching conversation, sending STOPPED for old"
                );
            }

            // Create new state with timers
            let refresh_timer = Timer::default();
            let pause_timer = Timer::default();

            // Send initial STARTED
            tracing::info!(
                conversation = ?conversation,
                "TYPING_SEND: Starting typing (first keystroke)"
            );

            let handle_send = handle.clone();
            tokio::spawn(async move {
                let _ = handle_send.send_typing(true).await;
            });

            // Set up refresh timer (re-sends STARTED every 10s)
            setup_refresh_timer(&refresh_timer, &handle);

            // Set up pause timer (sends STOPPED after 3s)
            setup_pause_timer(&pause_timer, conversation, &handle);

            *state_opt = Some(OutgoingTypingState {
                _refresh_timer: refresh_timer,
                pause_timer,
                last_bump: Instant::now(),
                conversation,
            });
        } else {
            // Same conversation — just reset pause timer (throttled)
            let state = state_opt.as_mut().unwrap();
            let now = Instant::now();

            if now.duration_since(state.last_bump) < Duration::from_millis(TYPING_THROTTLE_MS) {
                return;
            }

            state.last_bump = now;

            // Reset pause timer
            setup_pause_timer(&state.pause_timer, conversation, &handle);
        }
    });
}

/// Set up refresh timer — re-sends STARTED every 10s.
fn setup_refresh_timer(timer: &Timer, handle: &ConversationHandle) {
    let handle = handle.clone();

    timer.start(
        TimerMode::Repeated,
        Duration::from_millis(TYPING_REFRESH_MS),
        move || {
            let handle = handle.clone();
            tokio::spawn(async move {
                let _ = handle.send_typing(true).await;
            });
        },
    );
}

/// Set up pause timer — sends STOPPED after 3s and clears state.
fn setup_pause_timer(timer: &Timer, conversation: ActiveConversation, handle: &ConversationHandle) {
    let handle = handle.clone();

    timer.start(
        TimerMode::SingleShot,
        Duration::from_millis(TYPING_PAUSE_MS),
        move || {
            tracing::debug!(
                conversation = ?conversation,
                "TYPING_SEND: Pause timer fired - sending STOPPED"
            );

            OUTGOING_TYPING.with(|state_cell| {
                let state = state_cell.borrow_mut().take();
                if let Some(state) = state {
                    if state.conversation == conversation {
                        let handle = handle.clone();
                        tokio::spawn(async move {
                            let _ = handle.send_typing(false).await;
                        });
                    } else {
                        // Conversation changed, put state back
                        *state_cell.borrow_mut() = Some(state);
                    }
                }
            });
        },
    );
}

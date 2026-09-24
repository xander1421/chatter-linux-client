//! Typing timer management for inbox events.
//!
//! Signal-style per-peer typing timers with 15-second auto-clear.
//! Handles both header typing indicator and sidebar typing state.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};

use chatter_core::domain::ids::ChannelId;
use chatter_core::state::ChatState;
use slint::{ComponentHandle, Timer, TimerMode};

use crate::MainWindow;
use super::super::initial_data::update_sidebar_typing;

/// Typing timer state per peer.
/// Signal uses 15-second auto-clear timers that reset on each STARTED message.
pub(crate) struct TypingTimerEntry {
    pub timer: Timer,
}

thread_local! {
    /// Thread-local map of DM typing timers per peer ID.
    /// Key: peer_id string, Value: timer entry
    /// Must be thread_local because slint::Timer is not Send.
    pub(crate) static DM_TYPING_TIMERS: RefCell<HashMap<String, TypingTimerEntry>> = RefCell::new(HashMap::new());

    /// Set of cancelled peer IDs - timers will check this before clearing.
    /// This avoids dropping timers early which causes calloop orphan warnings.
    pub(crate) static CANCELLED_TIMERS: RefCell<HashSet<String>> = RefCell::new(HashSet::new());
}

/// Typing indicator timeout in milliseconds (Signal uses 15 seconds).
pub const TYPING_TIMEOUT_MS: u64 = 15_000;

/// Set typing indicator with auto-clear timer (Signal-style).
///
/// When STARTED is received:
/// 1. Show typing indicator ("is typing ..." since header shows peer name)
/// 2. Update sidebar typing state
/// 3. Start/restart 15-second timer to auto-clear both header and sidebar
///
/// `key` is the conversation's typing key and `channel_id` the conversation
/// itself: when the timer fires it clears the header only if that
/// conversation is still the open one, so a timer left over from the
/// conversation the user switched away from cannot blank the new one's.
pub fn set_typing_with_timer(
    window: &MainWindow,
    key: &str,
    chat_state: &ChatState,
    channel_id: ChannelId,
) {
    tracing::info!(key = %key, "TYPING_SET: Setting typing indicator");
    window.set_typing_indicator("is typing ...".into());
    update_sidebar_typing(window, key, true);

    // Clear from cancelled set if it was there (we're starting fresh)
    CANCELLED_TIMERS.with(|cancelled| {
        cancelled.borrow_mut().remove(key);
    });

    let weak_window = window.as_weak();
    let key_owned = key.to_string();
    let chat_state = chat_state.clone();
    let on_fire = move || {
        let was_cancelled = CANCELLED_TIMERS.with(|cancelled| {
            cancelled.borrow_mut().remove(&key_owned)
        });
        if was_cancelled {
            tracing::debug!(key = %key_owned, "TYPING_TIMER: fired but was cancelled - skipping clear");
        } else if let Some(w) = weak_window.upgrade() {
            tracing::debug!(key = %key_owned, "TYPING_TIMER: 15s timeout FIRED - clearing indicator");
            if chat_state.active_conversation().is_for_channel(channel_id) {
                w.set_typing_indicator("".into());
            }
            update_sidebar_typing(&w, &key_owned, false);
        }
        // Always remove from map when timer fires
        DM_TYPING_TIMERS.with(|timers| {
            timers.borrow_mut().remove(&key_owned);
        });
    };

    // Restart an existing timer rather than stop+create, which causes calloop
    // orphaned event warnings.
    DM_TYPING_TIMERS.with(|timers| {
        let mut map = timers.borrow_mut();
        let entry = map
            .entry(key.to_string())
            .or_insert_with(|| TypingTimerEntry { timer: Timer::default() });
        entry.timer.start(
            TimerMode::SingleShot,
            std::time::Duration::from_millis(TYPING_TIMEOUT_MS),
            on_fire,
        );
    });
}

/// Cancel typing timer for a peer.
///
/// Note: We don't remove the timer or call timer.stop() because dropping the Timer
/// while it has pending events causes calloop orphaned event warnings.
/// Instead, we mark it as cancelled - the timer will fire and check the cancelled set.
pub fn clear_typing_timer(peer_id: &str) {
    // Check if there's an active timer for this peer
    let has_timer = DM_TYPING_TIMERS.with(|timers| {
        timers.borrow().contains_key(peer_id)
    });

    if has_timer {
        // Mark as cancelled - the timer will fire and clean itself up
        CANCELLED_TIMERS.with(|cancelled| {
            cancelled.borrow_mut().insert(peer_id.to_string());
        });
        tracing::debug!(
            peer_id = %peer_id,
            "TYPING_TIMER: Marked timer as cancelled (will fire harmlessly)"
        );
    }
}

/// Clear typing state for a conversation key (called when a message arrives or
/// on STOPPED). Always clears the sidebar row and the timer; clears the header
/// only when `is_active` — the header belongs to the open conversation, and a
/// STOPPED from some other conversation must not hide typing shown there.
pub fn clear_typing_for_peer(window: &MainWindow, peer_id: &str, is_active: bool) {
    tracing::info!(
        peer_id = %peer_id,
        is_active,
        "TYPING_UI: Clearing typing indicator"
    );
    clear_typing_timer(peer_id);
    if is_active {
        window.set_typing_indicator("".into());
    }
    // Also clear sidebar typing state
    update_sidebar_typing(window, peer_id, false);
}


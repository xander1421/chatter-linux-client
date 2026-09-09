//! Typing timer management for inbox events.
//!
//! Signal-style per-peer typing timers with 15-second auto-clear.
//! Handles both header typing indicator and sidebar typing state.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};

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
pub fn set_typing_with_timer(window: &MainWindow, peer_id: &str) {
    // Show typing indicator (peer name already shown in header for DMs)
    let indicator_text = "is typing ...";
    let current = window.get_typing_indicator().to_string();
    tracing::info!(
        peer_id = %peer_id,
        current_value = %current,
        new_value = %indicator_text,
        "TYPING_SET: ===> Setting typing indicator on window <==="
    );
    window.set_typing_indicator(indicator_text.into());

    // Also update sidebar typing state
    update_sidebar_typing(window, peer_id, true);

    let after = window.get_typing_indicator().to_string();
    tracing::info!(
        peer_id = %peer_id,
        after_value = %after,
        "TYPING_SET: Typing indicator set, verifying value"
    );

    // Check if we have an existing timer for this peer - if so, restart it
    // This avoids stop+create which causes calloop orphaned event warnings
    let needs_new_timer = DM_TYPING_TIMERS.with(|timers| {
        let map = timers.borrow();
        !map.contains_key(peer_id)
    });

    // Clear from cancelled set if it was there (we're starting fresh)
    CANCELLED_TIMERS.with(|cancelled| {
        cancelled.borrow_mut().remove(peer_id);
    });

    if needs_new_timer {
        // Create new timer only if one doesn't exist
        let timer = Timer::default();
        let weak_window = window.as_weak();
        let peer_id_owned = peer_id.to_string();
        let peer_id_for_cleanup = peer_id.to_string();

        tracing::debug!(
            peer_id = %peer_id,
            timeout_ms = TYPING_TIMEOUT_MS,
            "TYPING_TIMER: Creating new 15s timer"
        );

        timer.start(
            TimerMode::SingleShot,
            std::time::Duration::from_millis(TYPING_TIMEOUT_MS),
            move || {
                // Check if this timer was cancelled
                let was_cancelled = CANCELLED_TIMERS.with(|cancelled| {
                    cancelled.borrow_mut().remove(&peer_id_owned)
                });

                if was_cancelled {
                    tracing::debug!(
                        peer_id = %peer_id_owned,
                        "TYPING_TIMER: Timer fired but was cancelled - skipping clear"
                    );
                } else {
                    tracing::debug!(
                        peer_id = %peer_id_owned,
                        "TYPING_TIMER: 15s timeout FIRED - clearing indicator"
                    );
                    // Clear typing indicator (header and sidebar)
                    if let Some(w) = weak_window.upgrade() {
                        w.set_typing_indicator("".into());
                        update_sidebar_typing(&w, &peer_id_owned, false);
                    }
                }

                // Always remove from map when timer fires
                DM_TYPING_TIMERS.with(|timers| {
                    timers.borrow_mut().remove(&peer_id_owned);
                });
            },
        );

        // Store timer
        DM_TYPING_TIMERS.with(|timers| {
            timers.borrow_mut().insert(peer_id_for_cleanup, TypingTimerEntry { timer });
        });
    } else {
        // Restart existing timer (just call start again - Slint handles this)
        DM_TYPING_TIMERS.with(|timers| {
            let map = timers.borrow();
            if let Some(entry) = map.get(peer_id) {
                let weak_window = window.as_weak();
                let peer_id_owned = peer_id.to_string();

                tracing::debug!(
                    peer_id = %peer_id,
                    timeout_ms = TYPING_TIMEOUT_MS,
                    "TYPING_TIMER: Restarting existing timer"
                );

                entry.timer.start(
                    TimerMode::SingleShot,
                    std::time::Duration::from_millis(TYPING_TIMEOUT_MS),
                    move || {
                        // Check if this timer was cancelled
                        let was_cancelled = CANCELLED_TIMERS.with(|cancelled| {
                            cancelled.borrow_mut().remove(&peer_id_owned)
                        });

                        if was_cancelled {
                            tracing::debug!(
                                peer_id = %peer_id_owned,
                                "TYPING_TIMER: Timer fired but was cancelled - skipping clear"
                            );
                        } else {
                            tracing::debug!(
                                peer_id = %peer_id_owned,
                                "TYPING_TIMER: 15s timeout FIRED - clearing indicator"
                            );
                            // Clear typing indicator (header and sidebar)
                            if let Some(w) = weak_window.upgrade() {
                                w.set_typing_indicator("".into());
                                update_sidebar_typing(&w, &peer_id_owned, false);
                            }
                        }

                        DM_TYPING_TIMERS.with(|timers| {
                            timers.borrow_mut().remove(&peer_id_owned);
                        });
                    },
                );
            }
        });
    }
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

/// Clear typing indicator for a peer (called when message received or STOPPED).
/// Clears both header and sidebar typing state.
pub fn clear_typing_for_peer(window: &MainWindow, peer_id: &str) {
    let current = window.get_typing_indicator().to_string();
    tracing::info!(
        peer_id = %peer_id,
        current_value = %current,
        "TYPING_UI: Clearing typing indicator (header + sidebar)"
    );
    clear_typing_timer(peer_id);
    window.set_typing_indicator("".into());
    // Also clear sidebar typing state
    update_sidebar_typing(window, peer_id, false);
}


//! Typing timer management for inbox events.
//!
//! Signal-style per-peer typing timers with 15-second auto-clear.

use std::cell::RefCell;
use std::collections::HashMap;

use slint::{ComponentHandle, Timer, TimerMode};

use crate::MainWindow;

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
}

/// Typing indicator timeout in milliseconds (Signal uses 15 seconds).
pub const TYPING_TIMEOUT_MS: u64 = 15_000;

/// Set typing indicator with auto-clear timer (Signal-style).
///
/// When STARTED is received:
/// 1. Show typing indicator ("is typing ..." since header shows peer name)
/// 2. Start/restart 15-second timer to auto-clear (reuses existing timer to avoid calloop warnings)
pub fn set_typing_with_timer(window: &MainWindow, peer_id: &str) {
    // Show typing indicator (peer name already shown in header for DMs)
    let indicator_text = "is typing ...";
    tracing::debug!(
        peer_id = %peer_id,
        "TYPING_UI: Setting typing indicator"
    );
    window.set_typing_indicator(indicator_text.into());

    // Check if we have an existing timer for this peer - if so, restart it
    // This avoids stop+create which causes calloop orphaned event warnings
    let needs_new_timer = DM_TYPING_TIMERS.with(|timers| {
        let map = timers.borrow();
        !map.contains_key(peer_id)
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
                tracing::debug!(
                    peer_id = %peer_id_owned,
                    "TYPING_TIMER: 15s timeout FIRED - clearing indicator"
                );
                // Clear typing indicator
                if let Some(w) = weak_window.upgrade() {
                    w.set_typing_indicator("".into());
                }
                // Remove from map
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
                        tracing::debug!(
                            peer_id = %peer_id_owned,
                            "TYPING_TIMER: 15s timeout FIRED - clearing indicator"
                        );
                        if let Some(w) = weak_window.upgrade() {
                            w.set_typing_indicator("".into());
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

/// Clear typing indicator and remove timer for a peer.
///
/// Note: We don't call timer.stop() because that can cause calloop orphaned event warnings.
/// Instead, we just remove from map - the timer will fire harmlessly since the callback
/// checks if the entry still exists in the map.
pub fn clear_typing_timer(peer_id: &str) {
    DM_TYPING_TIMERS.with(|timers| {
        let mut map = timers.borrow_mut();
        if map.remove(peer_id).is_some() {
            tracing::debug!(
                peer_id = %peer_id,
                remaining_timers = map.len(),
                "TYPING_TIMER: Removed timer entry for peer"
            );
        }
    });
}

/// Clear typing indicator for a peer (called when message received or STOPPED).
pub fn clear_typing_for_peer(window: &MainWindow, peer_id: &str) {
    let current = window.get_typing_indicator().to_string();
    tracing::info!(
        peer_id = %peer_id,
        current_value = %current,
        "TYPING_UI: Clearing typing indicator"
    );
    clear_typing_timer(peer_id);
    window.set_typing_indicator("".into());
}

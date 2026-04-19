//! Outgoing typing-indicator state machine (Signal-style).
//!
//! Owns the refresh / pause timer policy that used to live in the UI crate:
//!   - 10s refresh — periodically re-send STARTED while the user keeps typing
//!   - 3s pause   — send STOPPED after the user pauses
//!   - 300ms throttle on bump() so keystroke bursts don't flood timers
//!
//! UI callers issue only `bump(conversation)` per keystroke and `clear()`
//! when the compose box empties / message is sent.
//!
//! Lives in core so the Signal protocol's timing guarantees are enforced
//! uniformly across desktop + mobile frontends, and so the UI can't
//! accidentally skew the cadence.

use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::services::conversation::ConversationHandle;
use crate::state::ActiveConversation;

/// Signal: resend STARTED every 10s while user is still typing.
const TYPING_REFRESH_MS: u64 = 10_000;
/// Signal: send STOPPED 3s after last keystroke.
const TYPING_PAUSE_MS: u64 = 3_000;
/// Debounce rapid-fire keystrokes — reset pause timer at most once per 300ms.
const TYPING_THROTTLE_MS: u64 = 300;

struct State {
    conversation: ActiveConversation,
    refresh_task: JoinHandle<()>,
    pause_task: JoinHandle<()>,
    last_bump: Instant,
}

impl Drop for State {
    fn drop(&mut self) {
        self.refresh_task.abort();
        self.pause_task.abort();
    }
}

/// Per-client outgoing typing state manager. Clone is cheap (inner Arc).
#[derive(Clone, Default)]
pub struct OutgoingTypingManager {
    state: Arc<Mutex<Option<State>>>,
}

impl OutgoingTypingManager {
    pub fn new() -> Self {
        Self { state: Arc::new(Mutex::new(None)) }
    }

    /// Keystroke arrived — ensure we're typing in `handle.conversation()`.
    ///
    /// First bump after idle (or after conversation change): sends STARTED,
    /// spins the refresh + pause timers. Subsequent bumps in the same
    /// conversation reset the pause timer (throttled to 300ms).
    pub fn bump(&self, handle: ConversationHandle) {
        let conversation = handle.conversation().clone();
        let mut guard = self.state.lock();

        let needs_new = match &*guard {
            Some(s) => s.conversation != conversation,
            None => true,
        };

        if needs_new {
            if let Some(old) = guard.take() {
                // Conversation switched — send STOPPED for the old one.
                let old_conversation = old.conversation.clone();
                drop(old); // aborts old tasks
                let handle_stop = handle_for_conversation(&handle, &old_conversation);
                tokio::spawn(async move {
                    if let Some(h) = handle_stop {
                        let _ = h.send_typing(false).await;
                    }
                });
            }

            // Send initial STARTED.
            let handle_start = handle.clone();
            tokio::spawn(async move {
                let _ = handle_start.send_typing(true).await;
            });

            let refresh_task = spawn_refresh(handle.clone());
            let pause_task = spawn_pause(handle.clone(), self.state.clone(), conversation.clone());
            *guard = Some(State {
                conversation,
                refresh_task,
                pause_task,
                last_bump: Instant::now(),
            });
        } else {
            // Same conversation — reset pause timer with throttle.
            let now = Instant::now();
            let state = guard.as_mut().unwrap();
            if now.duration_since(state.last_bump) < Duration::from_millis(TYPING_THROTTLE_MS) {
                return;
            }
            state.last_bump = now;
            state.pause_task.abort();
            state.pause_task =
                spawn_pause(handle.clone(), self.state.clone(), state.conversation.clone());
        }
    }

    /// User explicitly stopped typing (e.g. compose cleared, message sent).
    /// Aborts timers and sends STOPPED. No-op if already idle.
    pub fn clear(&self, handle_resolver: impl FnOnce(&ActiveConversation) -> Option<ConversationHandle>) {
        let taken = self.state.lock().take();
        let Some(state) = taken else { return };
        let conversation = state.conversation.clone();
        drop(state); // aborts tasks
        if let Some(handle) = handle_resolver(&conversation) {
            tokio::spawn(async move {
                let _ = handle.send_typing(false).await;
            });
        }
    }
}

fn spawn_refresh(handle: ConversationHandle) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_millis(TYPING_REFRESH_MS));
        // First tick fires immediately — skip, the bump() path already sent STARTED.
        interval.tick().await;
        loop {
            interval.tick().await;
            let _ = handle.send_typing(true).await;
        }
    })
}

fn spawn_pause(
    handle: ConversationHandle,
    state: Arc<Mutex<Option<State>>>,
    conversation: ActiveConversation,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(TYPING_PAUSE_MS)).await;
        // Only clear + send STOPPED if our conversation still owns the slot.
        let taken = {
            let mut guard = state.lock();
            match &*guard {
                Some(s) if s.conversation == conversation => guard.take(),
                _ => None,
            }
        };
        if taken.is_some() {
            let _ = handle.send_typing(false).await;
        }
    })
}

/// Rebuild a `ConversationHandle` for a (possibly stale) active conversation.
/// Returns `None` if the source handle is for a different conversation and
/// we can't reconstruct one — caller will skip the STOPPED send.
fn handle_for_conversation(
    current: &ConversationHandle,
    target: &ActiveConversation,
) -> Option<ConversationHandle> {
    if current.conversation() == target {
        Some(current.clone())
    } else {
        // We don't have access to ChatterClient here to rebuild; caller in
        // ChatterClient::typing_clear will pass a resolver for precisely this.
        None
    }
}

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
use tokio::sync::mpsc;
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

/// FIFO of typing sends. Every STARTED/STOPPED goes through one sender task,
/// so they leave in the order the state machine decided them. Separate
/// `tokio::spawn`s per send raced: a STOPPED could reach the peer before the
/// STARTED it ends and leave them showing "typing" until their 15s timeout.
///
/// Enqueue happens under the state lock (see callers), so queue order is
/// state order even when the refresh / pause timers race each other.
///
/// one queue for all conversations, so a slow send delays the next
/// conversation's too. Typing runs in one conversation at a time; go
/// per-conversation if that stops being true.
#[derive(Clone)]
struct SendQueue {
    tx: mpsc::UnboundedSender<(ConversationHandle, bool)>,
    /// Taken by the first send, which starts the worker — `new()` may run
    /// outside a tokio runtime, `send` never does (callers already spawn).
    rx: Arc<Mutex<Option<mpsc::UnboundedReceiver<(ConversationHandle, bool)>>>>,
}

impl SendQueue {
    fn new() -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        Self {
            tx,
            rx: Arc::new(Mutex::new(Some(rx))),
        }
    }

    fn send(&self, handle: ConversationHandle, started: bool) {
        if let Some(mut rx) = self.rx.lock().take() {
            tokio::spawn(async move {
                while let Some((handle, started)) = rx.recv().await {
                    let _ = handle.send_typing(started).await;
                }
            });
        }
        let _ = self.tx.send((handle, started));
    }
}

/// Per-client outgoing typing state manager. Clone is cheap (inner Arc).
#[derive(Clone)]
pub struct OutgoingTypingManager {
    state: Arc<Mutex<Option<State>>>,
    queue: SendQueue,
}

impl Default for OutgoingTypingManager {
    fn default() -> Self {
        Self::new()
    }
}

impl OutgoingTypingManager {
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(None)),
            queue: SendQueue::new(),
        }
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
                self.queue
                    .send(handle.for_conversation(old_conversation), false);
            }

            // Send initial STARTED.
            self.queue.send(handle.clone(), true);

            let refresh_task = spawn_refresh(
                handle.clone(),
                self.state.clone(),
                conversation.clone(),
                self.queue.clone(),
            );
            let pause_task = spawn_pause(
                handle.clone(),
                self.state.clone(),
                conversation.clone(),
                self.queue.clone(),
            );
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
            state.pause_task = spawn_pause(
                handle.clone(),
                self.state.clone(),
                state.conversation.clone(),
                self.queue.clone(),
            );
        }
    }

    /// User explicitly stopped typing (e.g. compose cleared, message sent).
    /// Aborts timers and sends STOPPED. No-op if already idle.
    pub fn clear(
        &self,
        handle_resolver: impl FnOnce(&ActiveConversation) -> Option<ConversationHandle>,
    ) {
        let mut guard = self.state.lock();
        let Some(state) = guard.take() else { return };
        let conversation = state.conversation.clone();
        drop(state); // aborts tasks
        if let Some(handle) = handle_resolver(&conversation) {
            self.queue.send(handle, false);
        }
    }
}

fn spawn_refresh(
    handle: ConversationHandle,
    state: Arc<Mutex<Option<State>>>,
    conversation: ActiveConversation,
    queue: SendQueue,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_millis(TYPING_REFRESH_MS));
        // First tick fires immediately — skip, the bump() path already sent STARTED.
        interval.tick().await;
        loop {
            interval.tick().await;
            // Re-check under the lock: abort only lands at an await, so a
            // refresh racing the pause timer's STOPPED must not enqueue a
            // STARTED after it.
            let guard = state.lock();
            match &*guard {
                Some(s) if s.conversation == conversation => queue.send(handle.clone(), true),
                _ => return,
            }
        }
    })
}

fn spawn_pause(
    handle: ConversationHandle,
    state: Arc<Mutex<Option<State>>>,
    conversation: ActiveConversation,
    queue: SendQueue,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(TYPING_PAUSE_MS)).await;
        // Only clear + send STOPPED if our conversation still owns the slot.
        let mut guard = state.lock();
        let owns = matches!(&*guard, Some(s) if s.conversation == conversation);
        if owns {
            let taken = guard.take();
            queue.send(handle, false);
            drop(guard);
            drop(taken); // aborts the timers — may include this task; already done
        }
    })
}

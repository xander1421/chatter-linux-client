//! Reaction callbacks for DM and channel conversations.

use std::sync::Arc;

use slint::{ComponentHandle, SharedString};

use chatter_core::client::ChatterClient;
use chatter_core::state::ActiveConversation;

use crate::{MainWindow, run_ui};

/// Wire reaction selected callback for DM and channel conversations.
pub fn wire_reaction_selected(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let ui_ctx = ui_ctx.clone();
    let weak_window = window.as_weak();
    // Enqueue on the callback thread before any task can be scheduled. A single
    // consumer decides toggles in callback order, including after failed sends.
    let (selection_queue, intents) = tokio::sync::mpsc::unbounded_channel::<ReactionIntent>();
    let client = ui_ctx.clone();
    tokio::spawn(async move {
        consume_fifo(intents, move |intent| {
            let client = client.clone();
            let weak_window = weak_window.clone();
            async move {
                let ReactionIntent {
                    conversation,
                    scope,
                    owner,
                    session_revision,
                    target_author,
                    target_sent_timestamp,
                    emoji,
                } = intent;
                let auth = client.auth_state().get();
                if !reaction_session_is_current(
                    owner,
                    session_revision,
                    auth.current_user_id,
                    auth.session_revision,
                ) {
                    tracing::debug!("Dropping reaction intent from previous auth session");
                    return;
                }
                let handle = client.conversation(conversation.clone());
                if let Err(e) = handle
                    .toggle_reaction_for_scope(
                        &scope,
                        &emoji,
                        &target_author,
                        target_sent_timestamp,
                    )
                    .await
                {
                    tracing::error!(error = %e, conversation_type = ?conversation,
                    sender_id = %target_author, timestamp = target_sent_timestamp,
                    "Failed to toggle reaction");
                    return;
                }
                let auth = client.auth_state().get();
                if !reaction_session_is_current(
                    owner,
                    session_revision,
                    auth.current_user_id,
                    auth.session_revision,
                ) {
                    return;
                }
                let client = client.clone();
                let weak = weak_window.clone();
                run_ui!(weak, |w| {
                    let auth = client.auth_state().get();
                    if !reaction_session_is_current(
                        owner,
                        session_revision,
                        auth.current_user_id,
                        auth.session_revision,
                    ) {
                        return;
                    }
                    super::helpers::refresh_scoped_reaction_in_ui(
                        &w,
                        &client,
                        conversation,
                        &scope,
                        &target_author,
                        target_sent_timestamp,
                    );
                });
            }
        })
        .await;
    });

    window.on_reaction_selected(
        move |sender_id: SharedString,
              timestamp_ns: SharedString,
              _message_id: SharedString,
              emoji: SharedString| {
            let sender_id_str = sender_id.to_string();
            let timestamp_ns_str = timestamp_ns.to_string();
            let emoji_str = emoji.to_string();

            let target_sent_timestamp: u64 = match timestamp_ns_str.parse() {
                Ok(ts) => ts,
                Err(_) => return,
            };

            let conversation = ui_ctx.chat_state().active_conversation();
            if matches!(conversation, ActiveConversation::None) {
                return;
            }

            let target_author = match sender_id_str.parse() {
                Ok(id) => id,
                Err(_) => {
                    tracing::error!(sender_id = %sender_id_str, "invalid sender id for reaction");
                    return;
                }
            };
            let auth = ui_ctx.auth_state().get();
            let Some(owner) = auth.current_user_id else {
                return;
            };
            let session_revision = auth.session_revision;
            // Every reaction route (picker, quick bar, tapping a chip) lands
            // here, so this is where the account's recent emoji learn a pick —
            // under the owner and session captured for this very reaction.
            ui_ctx.record_recent_emoji(
                chatter_core::client::PickerHistoryScope {
                    owner,
                    session_revision,
                },
                &emoji_str,
            );
            let scope = match ui_ctx
                .conversation(conversation.clone())
                .reaction_snapshot(&target_author, target_sent_timestamp)
            {
                Ok(Some(snapshot)) => snapshot.scope,
                Ok(None) => return,
                Err(error) => {
                    tracing::error!(%error, "Cannot capture reaction target scope");
                    return;
                }
            };
            if selection_queue
                .send(ReactionIntent {
                    conversation,
                    scope,
                    owner,
                    session_revision,
                    target_author,
                    target_sent_timestamp,
                    emoji: emoji_str,
                })
                .is_err()
            {
                tracing::error!("Reaction intent consumer stopped");
            }
        },
    );
}

/// Captured target identity survives a conversation switch while transport waits.
struct ReactionIntent {
    conversation: ActiveConversation,
    scope: chatter_core::domain::reaction::ReactionScope,
    owner: chatter_core::domain::UserId,
    session_revision: u64,
    target_author: chatter_core::domain::UserId,
    target_sent_timestamp: u64,
    emoji: String,
}

fn reaction_session_is_current(
    owner: chatter_core::domain::UserId,
    revision: u64,
    current_owner: Option<chatter_core::domain::UserId>,
    current_revision: u64,
) -> bool {
    current_owner == Some(owner) && current_revision == revision
}

/// One receiver owns all work. Enqueue order precedes async scheduling.
async fn consume_fifo<T, F, Fut>(mut intents: tokio::sync::mpsc::UnboundedReceiver<T>, mut apply: F)
where
    F: FnMut(T) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    while let Some(intent) = intents.recv().await {
        apply(intent).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn callbacks_remain_fifo_with_slow_failure_and_switch() {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let (release, gate) = tokio::sync::oneshot::channel();
        let completed = Arc::new(std::sync::Mutex::new(Vec::new()));
        // Capture scope and enqueue synchronously, before the consumer starts.
        // Last two intents belong to a different conversation with same target.
        for intent in [
            ("old", "A", false),
            ("old", "B", false),
            ("old", "B", false),
            ("old", "A", true),
            ("new", "A", false),
            ("new", "A", false),
        ] {
            tx.send(intent).unwrap();
        }
        drop(tx);
        let results = completed.clone();
        let selections = Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
        let state = selections.clone();
        let mut first_gate = Some(gate);
        let consumer = tokio::spawn(consume_fifo(rx, move |intent| {
            let wait = first_gate.take();
            let results = results.clone();
            let selections = state.clone();
            async move {
                if let Some(wait) = wait {
                    wait.await.unwrap();
                }
                // A failed send still finishes its turn; later intents continue.
                let (scope, emoji, failed) = intent;
                if !failed {
                    let mut selected = selections.lock().unwrap();
                    if selected.get(scope) == Some(&emoji) {
                        selected.remove(scope);
                    } else {
                        selected.insert(scope, emoji);
                    }
                }
                let own = selections.lock().unwrap().get(scope).copied();
                results.lock().unwrap().push((intent, own));
            }
        }));
        tokio::task::yield_now().await;
        assert!(completed.lock().unwrap().is_empty());
        release.send(()).unwrap();
        consumer.await.unwrap();
        let seen = completed.lock().unwrap();
        assert_eq!(
            *seen,
            vec![
                (("old", "A", false), Some("A")),
                (("old", "B", false), Some("B")),
                (("old", "B", false), None),
                (("old", "A", true), None),
                (("new", "A", false), Some("A")),
                (("new", "A", false), None),
            ]
        );
        assert!(selections.lock().unwrap().is_empty());
    }
    #[tokio::test]
    async fn queued_old_session_is_rejected_after_a_slow_failed_send() {
        let owner = chatter_core::domain::UserId::new();
        let current = Arc::new(std::sync::Mutex::new((Some(owner), 1u64)));
        let dispatched = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release, wait) = tokio::sync::oneshot::channel();
        tx.send((owner, 1, "A")).unwrap();
        tx.send((owner, 1, "B")).unwrap();
        let active = current.clone();
        let sent = dispatched.clone();
        let mut gate = Some((started_tx, wait));
        let consumer = tokio::spawn(consume_fifo(rx, move |(actor, revision, emoji)| {
            let active = active.clone();
            let sent = sent.clone();
            let gate = gate.take();
            async move {
                let (current_owner, current_revision) = *active.lock().unwrap();
                if !reaction_session_is_current(actor, revision, current_owner, current_revision) {
                    return;
                }
                sent.lock().unwrap().push(emoji);
                if let Some((started, wait)) = gate {
                    started.send(()).unwrap();
                    wait.await.unwrap(); // Failed send completes after a session switch.
                }
            }
        }));
        started_rx.await.unwrap();
        // Same account logged in again: revision, rather than user ID alone,
        // fences the already queued B intent.
        *current.lock().unwrap() = (Some(owner), 2);
        tx.send((owner, 2, "C")).unwrap();
        drop(tx);
        release.send(()).unwrap();
        consumer.await.unwrap();
        assert_eq!(*dispatched.lock().unwrap(), vec!["A", "C"]);
        assert!(!reaction_session_is_current(owner, 2, None, 2));
        assert!(!reaction_session_is_current(
            owner,
            2,
            Some(chatter_core::domain::UserId::new()),
            2
        ));
    }
}

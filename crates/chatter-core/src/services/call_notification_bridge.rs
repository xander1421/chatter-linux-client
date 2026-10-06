//! Bridge: call orchestrator events → ringing OS notification.
//!
//! `CallWorker` emits its own `CallOrchestratorEvent` stream consumed by
//! the in-app call modal. This task watches the same stream and owns the
//! system ring notification: posted on `IncomingPresented`, cancelled on
//! `IncomingWithdrawn` — every way a presented call stops ringing without
//! being accepted (declined here or elsewhere, answered elsewhere, caller
//! hung up). Accepting emits no orchestrator event, so
//! `ChatterClient::accept_presented_incoming_call` cancels it itself.
//!
//! Show and cancel run in this one task, in event order: an offer and its
//! hangup delivered back to back from the queue can't cancel before the post.

#[cfg(feature = "media")]
use std::sync::Arc;
#[cfg(feature = "media")]
use tokio::sync::broadcast;

#[cfg(feature = "media")]
use crate::services::call_worker::CallOrchestratorEvent;
#[cfg(feature = "media")]
use crate::services::notification_service::NotificationService;

/// Spawn a task that posts the ring notification on `IncomingPresented` and
/// cancels it on `IncomingWithdrawn`. `rx` is
/// `CallWorkerHandle::subscribe_call_events`. Exits when the channel closes.
#[cfg(feature = "media")]
pub fn spawn_call_notification_bridge(
    mut rx: broadcast::Receiver<CallOrchestratorEvent>,
    notifications: Arc<NotificationService>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(CallOrchestratorEvent::IncomingPresented {
                    call,
                    peer_name,
                    is_video,
                    channel_context,
                    ..
                }) => {
                    // Title carries the media flag so the user knows whether
                    // to expect video.
                    let title = if is_video {
                        "Incoming video call"
                    } else {
                        "Incoming call"
                    };
                    let body = match channel_context.as_deref() {
                        Some(ch) if !ch.is_empty() => format!("{} is calling in {}", peer_name, ch),
                        _ => format!("{} is calling", peer_name),
                    };
                    tracing::info!(call_id = %call.as_str(), is_video, "IncomingPresented → ringing call notification");
                    // A failure (NotificationService turns a sink panic into
                    // one) is this call's; the loop serves the next.
                    if let Err(e) = notifications.show_incoming_call(call.as_str(), title, &body) {
                        tracing::error!(call_id = %call.as_str(), error = %e, "ring notification failed");
                    }
                }
                Ok(CallOrchestratorEvent::IncomingWithdrawn { call, .. }) => {
                    if let Err(e) = notifications.cancel_incoming_call(call.as_str()) {
                        tracing::error!(call_id = %call.as_str(), error = %e, "ring notification cancel failed");
                    }
                }
                // Other orchestrator events don't touch the ring
                // notification — UI handles in-app state directly.
                Ok(_) => {}
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!(dropped_count = n, "call notification bridge lagged");
                }
                Err(broadcast::error::RecvError::Closed) => {
                    tracing::debug!("call notification bridge: channel closed, exiting");
                    break;
                }
            }
        }
    })
}

#[cfg(all(test, feature = "media"))]
mod tests {
    use super::*;
    use crate::domain::ids::UserId;
    use crate::domain::notification::NotificationPriority;
    use crate::error::SystemError;
    use crate::services::call_worker::{CallEndReason, CallHandle, OrchestratedCallKind};
    use crate::system::notification_sink::NotificationSink;
    use parking_lot::Mutex;

    #[derive(Default)]
    struct Log(Mutex<Vec<String>>);
    impl NotificationSink for Log {
        fn show(&self, _: &str, _: &str, _: NotificationPriority) -> Result<(), SystemError> {
            Ok(())
        }
        fn show_incoming_call(&self, call_id: &str, _: &str, _: &str) -> Result<(), SystemError> {
            self.0.lock().push(format!("show {call_id}"));
            Ok(())
        }
        fn cancel_incoming_call(&self, call_id: &str) -> Result<(), SystemError> {
            self.0.lock().push(format!("cancel {call_id}"));
            Ok(())
        }
    }

    fn presented(id: &str) -> CallOrchestratorEvent {
        CallOrchestratorEvent::IncomingPresented {
            call: CallHandle::from(id),
            kind: OrchestratedCallKind::DmStandard,
            peer_id: UserId::new(),
            peer_name: "Bob".into(),
            is_video: false,
            is_identity_verified: true,
            identity_warning: String::new(),
            ephemeral_public_key: None,
            channel_context: None,
            conversation_id: String::new(),
            replaces_active: false,
        }
    }

    /// An offer and its hangup delivered back to back still post before
    /// they cancel, and a withdrawal cancels the notification of that call.
    #[tokio::test]
    async fn withdrawn_call_cancels_its_ring_notification() {
        let log = Arc::new(Log::default());
        let (tx, rx) = broadcast::channel(8);
        let task =
            spawn_call_notification_bridge(rx, Arc::new(NotificationService::new(log.clone())));
        tx.send(presented("c1")).unwrap();
        tx.send(CallOrchestratorEvent::IncomingWithdrawn {
            call: CallHandle::from("c1"),
            reason: CallEndReason::RemoteHangup,
        })
        .unwrap();
        drop(tx);
        task.await.unwrap();
        assert_eq!(*log.0.lock(), ["show c1", "cancel c1"]);
    }
    #[tokio::test]
    async fn ended_group_ring_cancels_native_notification_without_canceling_new_call() {
        let log = Arc::new(Log::default());
        let (tx, rx) = broadcast::channel(8);
        let task =
            spawn_call_notification_bridge(rx, Arc::new(NotificationService::new(log.clone())));
        let mut group = presented("group-old");
        if let CallOrchestratorEvent::IncomingPresented {
            kind,
            channel_context,
            ..
        } = &mut group
        {
            *kind = OrchestratedCallKind::GroupChannel;
            *channel_context = Some("Shared room".into());
        }
        tx.send(group).unwrap();
        tx.send(presented("new-call")).unwrap();
        tx.send(CallOrchestratorEvent::IncomingWithdrawn {
            call: CallHandle::from("group-old"),
            reason: CallEndReason::RemoteHangup,
        })
        .unwrap();
        drop(tx);
        task.await.unwrap();
        assert_eq!(
            *log.0.lock(),
            ["show group-old", "show new-call", "cancel group-old"]
        );
    }
}

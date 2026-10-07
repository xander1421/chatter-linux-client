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
//! The worker publishes key-free presentation authority. Buffered events are
//! invalidation hints, and startup/lag repair reads current authority; a call
//! already withdrawn before consumption is never posted from its old offer.

#[cfg(feature = "media")]
use std::sync::Arc;
#[cfg(feature = "media")]
use tokio::sync::broadcast;

#[cfg(feature = "media")]
use crate::services::call_worker::CallOrchestratorEvent;
#[cfg(feature = "media")]
use crate::services::notification_service::NotificationService;

/// Subscribe before reading presentation authority. Broadcast events only
/// invalidate the projection; their buffered payloads are never replayed.
#[cfg(feature = "media")]
pub fn spawn_call_notification_bridge(
    worker: crate::services::call_worker::CallWorkerHandle,
    notifications: Arc<NotificationService>,
) -> tokio::task::JoinHandle<()> {
    let rx = worker.subscribe_call_events();
    let state = worker.incoming_presentation_state();
    let changes = state.subscribe_changes();
    tokio::spawn(run_bridge(
        rx,
        state,
        changes,
        notifications,
        Default::default(),
    ))
}

#[cfg(feature = "media")]
async fn run_bridge(
    mut rx: broadcast::Receiver<CallOrchestratorEvent>,
    state: crate::services::call_worker::IncomingCallPresentationState,
    mut changes: tokio::sync::watch::Receiver<u64>,
    notifications: Arc<NotificationService>,
    mut posted: std::collections::HashSet<String>,
) {
    // A watch receiver repairs transitions that emit no broadcast (acceptance).
    // Mark the generation seen before reading, so a racing update wakes us again.
    changes.borrow_and_update();
    reconcile(&state, &notifications, &mut posted);
    loop {
        tokio::select! {
            result = changes.changed() => {
                if result.is_err() { break; }
                changes.borrow_and_update();
                reconcile(&state, &notifications, &mut posted);
            }
            result = rx.recv() => {
                if !recover_event(result, &state, &notifications, &mut posted) { break; }
            }
        }
    }
}

#[cfg(feature = "media")]
fn recover_event(
    result: Result<CallOrchestratorEvent, broadcast::error::RecvError>,
    state: &crate::services::call_worker::IncomingCallPresentationState,
    notifications: &NotificationService,
    posted: &mut std::collections::HashSet<String>,
) -> bool {
    match result {
        Err(broadcast::error::RecvError::Closed) => return false,
        Err(broadcast::error::RecvError::Lagged(n)) => {
            tracing::warn!(
                dropped_count = n,
                "call notification bridge recovering from lag"
            );
        }
        Ok(_) => {}
    }
    reconcile(state, notifications, posted);
    true
}

#[cfg(feature = "media")]
fn reconcile(
    state: &crate::services::call_worker::IncomingCallPresentationState,
    notifications: &NotificationService,
    posted: &mut std::collections::HashSet<String>,
) {
    // Keep the lease through native show/cancel. Acceptance publishes under
    // the same lock before replying, so its explicit cancel cannot be
    // overtaken by a stale post. No signaling key is read by this projection.
    state.with_snapshot(|snapshot| {
        let ringing: std::collections::HashSet<_> = snapshot.presentations.iter().filter_map(|event| {
            match event {
                CallOrchestratorEvent::IncomingPresented { call, .. } => Some(call.as_str().to_owned()),
                _ => None,
            }
        }).collect();
        posted.retain(|call| {
            if ringing.contains(call) { return true; }
            if let Err(error) = notifications.cancel_incoming_call(call) {
                tracing::error!(call_id = %call, %error, "ring notification cancel failed");
                return true; // retry on the next invalidation
            }
            false
        });
        for event in &snapshot.presentations {
            if let CallOrchestratorEvent::IncomingPresented { call, peer_name, is_video, channel_context, .. } = event {
                if posted.contains(call.as_str()) { continue; }
                let title = if *is_video { "Incoming video call" } else { "Incoming call" };
                let body = match channel_context.as_deref() {
                    Some(ch) if !ch.is_empty() => format!("{} is calling in {}", peer_name, ch),
                    _ => format!("{} is calling", peer_name),
                };
                match notifications.show_incoming_call(call.as_str(), title, &body) {
                    Ok(()) => { posted.insert(call.as_str().to_owned()); }
                    Err(error) => tracing::error!(call_id = %call.as_str(), %error, "ring notification failed"),
                }
            }
        }
    });
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

    fn worker() -> crate::services::call_worker::CallWorkerHandle {
        let channel = tonic::transport::Endpoint::from_static("http://127.0.0.1:9").connect_lazy();
        crate::services::call_worker::CallWorkerHandle::spawn(
            crate::network::grpc::CallClient::new(
                channel.into(),
                crate::network::grpc::interceptors::AuthInterceptor::new(),
            ),
        )
    }

    async fn present(worker: &crate::services::call_worker::CallWorkerHandle, id: &str) {
        worker.orch_present_incoming(
            CallHandle::from(id),
            OrchestratedCallKind::GroupChannel,
            UserId::new(),
            "Bob".into(),
            false,
            true,
            String::new(),
            None,
            Some("Room".into()),
            String::new(),
        );
        worker.incoming_presentation_snapshot().await.unwrap();
    }

    /// Overflow the actual worker broadcast (64 slots), losing the final
    /// withdrawal behind unrelated events. Recovery uses the production
    /// projection against the real actor and a sink that records exact IDs.
    #[tokio::test]
    async fn broadcast_overflow_recovers_final_withdrawal_without_resurrection() {
        overflow_recovery(false).await;
    }

    #[tokio::test]
    async fn broadcast_overflow_preserves_newer_ring_and_unrelated_active_call() {
        overflow_recovery(true).await;
    }

    async fn overflow_recovery(newer: bool) {
        let worker = worker();
        let state = worker.incoming_presentation_state();
        let log = Arc::new(Log::default());
        let notifications = NotificationService::new(log.clone());
        let mut posted = std::collections::HashSet::new();
        present(&worker, "old").await;
        let mut rx = worker.subscribe_call_events();
        reconcile(&state, &notifications, &mut posted);
        worker
            .orch_withdraw_incoming_reliably(CallHandle::from("old"), CallEndReason::RemoteHangup)
            .await
            .unwrap();
        // This stale presentation is itself retained in the overflowing
        // buffer, while its following withdrawal is authoritative final state.
        present(&worker, "buffered-stale").await;
        worker
            .orch_withdraw_incoming_reliably(
                CallHandle::from("buffered-stale"),
                CallEndReason::RemoteHangup,
            )
            .await
            .unwrap();
        for n in 0..80 {
            worker.orch_mark_connected(
                CallHandle::from(format!("active-{n}")),
                OrchestratedCallKind::DmStandard,
                UserId::new(),
                false,
            );
            worker.incoming_presentation_snapshot().await.unwrap();
        }
        // Keep another stale presentation in the retained tail.
        present(&worker, "tail-stale").await;
        worker
            .orch_withdraw_incoming_reliably(
                CallHandle::from("tail-stale"),
                CallEndReason::RemoteHangup,
            )
            .await
            .unwrap();
        if newer {
            present(&worker, "new").await;
        }
        let overflow = rx.recv().await;
        assert!(matches!(
            overflow,
            Err(broadcast::error::RecvError::Lagged(_))
        ));
        assert!(recover_event(overflow, &state, &notifications, &mut posted));
        // Replaying retained event payloads must not resurrect either stale ID.
        while let Ok(event) = rx.try_recv() {
            assert!(recover_event(
                Ok(event),
                &state,
                &notifications,
                &mut posted
            ));
        }
        assert_eq!(
            worker.orch_active_kind().await,
            Some(OrchestratedCallKind::DmStandard)
        );
        if newer {
            assert_eq!(*log.0.lock(), ["show old", "cancel old", "show new"]);
            assert_eq!(posted, std::collections::HashSet::from(["new".to_owned()]));
        } else {
            assert_eq!(*log.0.lock(), ["show old", "cancel old"]);
            assert!(posted.is_empty());
        }
        worker.shutdown();
    }

    #[tokio::test]
    async fn startup_recovers_existing_ring_and_acceptance_invalidates_it() {
        let worker = worker();
        present(&worker, "startup").await;
        let state = worker.incoming_presentation_state();
        let mut changes = state.subscribe_changes();
        changes.borrow_and_update();
        let log = Arc::new(Log::default());
        let notifications = NotificationService::new(log.clone());
        let mut posted = std::collections::HashSet::new();
        reconcile(&state, &notifications, &mut posted);
        worker
            .orch_accept_incoming(CallHandle::from("startup"))
            .await
            .unwrap();
        changes.changed().await.unwrap();
        reconcile(&state, &notifications, &mut posted);
        assert_eq!(*log.0.lock(), ["show startup", "cancel startup"]);
        worker.shutdown();
    }

    struct BlockingLog {
        log: Log,
        entered: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
        release: Mutex<std::sync::mpsc::Receiver<()>>,
    }

    impl NotificationSink for BlockingLog {
        fn show(&self, _: &str, _: &str, _: NotificationPriority) -> Result<(), SystemError> {
            Ok(())
        }
        fn show_incoming_call(&self, call: &str, _: &str, _: &str) -> Result<(), SystemError> {
            if let Some(entered) = self.entered.lock().take() {
                entered.send(()).unwrap();
                self.release.lock().recv().unwrap();
            }
            self.log.0.lock().push(format!("show {call}"));
            Ok(())
        }
        fn cancel_incoming_call(&self, call: &str) -> Result<(), SystemError> {
            self.log.0.lock().push(format!("cancel {call}"));
            Ok(())
        }
    }

    /// Exercise the real actor against a native post paused inside the
    /// production projection. Accept cannot reply/cancel until the post is
    /// ordered before publication, and a buffered offer cannot post afterward.
    #[tokio::test]
    async fn acceptance_cannot_be_overtaken_by_a_paused_native_post() {
        let worker = worker();
        present(&worker, "race").await;
        let state = worker.incoming_presentation_state();
        let stale = worker
            .incoming_presentation_snapshot()
            .await
            .unwrap()
            .presentations
            .remove(0);
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let log = Arc::new(BlockingLog {
            log: Log::default(),
            entered: Mutex::new(Some(entered_tx)),
            release: Mutex::new(release_rx),
        });
        let notifications = Arc::new(NotificationService::new(log.clone()));
        let reader_state = state.clone();
        let reader_notifications = notifications.clone();
        let reader = std::thread::spawn(move || {
            let mut posted = std::collections::HashSet::new();
            reconcile(&reader_state, &reader_notifications, &mut posted);
            posted
        });
        entered_rx.await.unwrap();
        let acceptance = worker.orch_accept_incoming(CallHandle::from("race"));
        tokio::pin!(acceptance);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(20), &mut acceptance)
                .await
                .is_err()
        );
        release_tx.send(()).unwrap();
        let mut posted = reader.join().unwrap();
        acceptance.await.unwrap();
        // The high-level accept path's explicit native cancellation.
        notifications.cancel_incoming_call("race").unwrap();
        assert!(recover_event(
            Ok(stale),
            &state,
            &notifications,
            &mut posted
        ));
        assert_eq!(
            *log.log.0.lock(),
            ["show race", "cancel race", "cancel race"]
        );
        assert!(posted.is_empty());
        worker.shutdown();
    }
}

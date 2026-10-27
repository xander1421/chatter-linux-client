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

#[cfg(feature = "media")]
#[derive(Clone)]
pub struct SessionBridge {
    stop: tokio::sync::watch::Sender<bool>,
    done: tokio::sync::watch::Receiver<bool>,
}

#[cfg(feature = "media")]
impl SessionBridge {
    pub async fn wait_drained(&self) -> bool {
        let mut done = self.done.clone();
        while !*done.borrow_and_update() {
            if done.changed().await.is_err() {
                return false;
            }
        }
        true
    }

    pub async fn stop(&self) -> bool {
        self.stop.send_replace(true);
        tokio::time::timeout(std::time::Duration::from_secs(2), self.wait_drained())
            .await
            .unwrap_or(false)
    }
}

#[cfg(feature = "media")]
pub fn spawn_session_call_notification_bridge(
    worker: crate::services::call_worker::CallWorkerHandle,
    notifications: Arc<NotificationService>,
) -> SessionBridge {
    let rx = worker.subscribe_call_events();
    let state = worker.incoming_presentation_state().capture_session();
    let changes = state.subscribe_changes();
    let (stop, stop_rx) = tokio::sync::watch::channel(false);
    let (done_tx, done) = tokio::sync::watch::channel(false);
    tokio::spawn(async move {
        run_bridge(
            rx,
            state,
            changes,
            notifications,
            Default::default(),
            Some(stop_rx),
        )
        .await;
        done_tx.send_replace(true);
    });
    SessionBridge { stop, done }
}

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
        None,
    ))
}

#[cfg(feature = "media")]
async fn run_bridge(
    mut rx: broadcast::Receiver<CallOrchestratorEvent>,
    state: crate::services::call_worker::IncomingCallPresentationState,
    mut changes: tokio::sync::watch::Receiver<u64>,
    notifications: Arc<NotificationService>,
    mut posted: std::collections::HashSet<String>,
    mut stop: Option<tokio::sync::watch::Receiver<bool>>,
) {
    // A watch receiver repairs transitions that emit no broadcast (acceptance).
    // Mark the generation seen before reading, so a racing update wakes us again.
    changes.borrow_and_update();
    reconcile(&state, &notifications, &mut posted);
    let mut retry = tokio::time::interval(std::time::Duration::from_millis(100));
    loop {
        tokio::select! {
            biased;
            _ = async {
                match &mut stop {
                    Some(rx) => {
                        while !*rx.borrow_and_update() {
                            if rx.changed().await.is_err() { break; }
                        }
                    }
                    None => std::future::pending::<()>().await,
                }
            } => break,
            result = changes.changed() => {
                if result.is_err() { break; }
                changes.borrow_and_update();
                reconcile(&state, &notifications, &mut posted);
            }
            _ = retry.tick(), if !posted.is_empty() => reconcile(&state, &notifications, &mut posted),
            result = rx.recv() => {
                if !recover_event(result, &state, &notifications, &mut posted) { break; }
            }
        }
    }
    // Every exit path owns its posts until native dismissal succeeds. The
    // event stream can close before the session teardown asks us to stop.
    while !posted.is_empty() {
        posted.retain(|call| notifications.cancel_incoming_call(call).is_err());
        if !posted.is_empty() {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
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
        if posted.iter().any(|call| !ringing.contains(call)) {
            tracing::warn!(target: "chatter_core::ring_lifecycle::withdrawn", "ring left the presentation");
        }
        posted.retain(|call| {
            if ringing.contains(call) { return true; }
            if let Err(error) = notifications.cancel_incoming_call(call) {
                tracing::error!(call_id = %call, %error, "ring notification cancel failed");
                return true; // retry on the next invalidation
            }
            tracing::warn!(target: "chatter_core::ring_lifecycle::cancelled", "ring notification cancelled");
            false
        });
        for event in &snapshot.presentations {
            if let CallOrchestratorEvent::IncomingPresented { call, peer_name, is_video, channel_context, .. } = event {
                if posted.contains(call.as_str()) { continue; }
                let title = if *is_video { "Incoming video call" } else { "Incoming call" };
                let body = match channel_context.as_deref() {
                    Some(ch) if !ch.is_empty() && notifications.detail_level() == super::notification_service::NotificationDetailLevel::NameAndMessage => format!("{} is calling in {}", peer_name, ch),
                    _ => format!("{} is calling", peer_name),
                };
                match notifications.show_incoming_call(call.as_str(), title, &body) {
                    Ok(()) => {
                        tracing::warn!(target: "chatter_core::ring_lifecycle::posted", "ring notification posted");
                        posted.insert(call.as_str().to_owned());
                    }
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

    async fn wait_for(log: &Log, entry: &str) {
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                if log.0.lock().iter().any(|item| item == entry) {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn spawned_session_bridge_recovers_startup_and_watch_only_accept() {
        let worker = worker();
        worker.incoming_presentation_state().begin_session(1);
        let scoped = worker.for_presentation_session(1);
        present(&scoped, "startup-spawned").await;
        let log = Arc::new(Log::default());
        let bridge = spawn_session_call_notification_bridge(
            scoped.clone(),
            Arc::new(NotificationService::new(log.clone())),
        );
        wait_for(&log, "show startup-spawned").await;
        scoped
            .orch_accept_incoming(CallHandle::from("startup-spawned"))
            .await
            .unwrap();
        wait_for(&log, "cancel startup-spawned").await;
        assert!(bridge.stop().await);
        assert_eq!(
            *log.0.lock(),
            ["show startup-spawned", "cancel startup-spawned"]
        );
        worker.shutdown();
    }

    #[tokio::test]
    async fn closed_worker_drains_the_ring_it_posted() {
        let worker = worker();
        present(&worker, "closing").await;
        let log = Arc::new(Log::default());
        let bridge = spawn_session_call_notification_bridge(
            worker.clone(),
            Arc::new(NotificationService::new(log.clone())),
        );
        wait_for(&log, "show closing").await;
        worker.shutdown();
        drop(worker);
        assert!(bridge.wait_drained().await);
        assert_eq!(*log.0.lock(), ["show closing", "cancel closing"]);
    }

    #[tokio::test]
    async fn group_join_and_withdrawal_cancel_their_exact_notifications() {
        let worker = worker();
        let log = Arc::new(Log::default());
        let bridge = spawn_session_call_notification_bridge(
            worker.clone(),
            Arc::new(NotificationService::new(log.clone())),
        );
        present(&worker, "joined").await;
        wait_for(&log, "show joined").await;
        worker.orch_mark_connected(
            CallHandle::from("joined"),
            OrchestratedCallKind::GroupChannel,
            UserId::new(),
            false,
        );
        worker.incoming_presentation_snapshot().await.unwrap();
        wait_for(&log, "cancel joined").await;
        present(&worker, "withdrawn").await;
        wait_for(&log, "show withdrawn").await;
        worker
            .orch_withdraw_incoming_reliably(
                CallHandle::from("withdrawn"),
                CallEndReason::RemoteHangup,
            )
            .await
            .unwrap();
        wait_for(&log, "cancel withdrawn").await;
        assert!(bridge.stop().await);
        assert_eq!(
            *log.0.lock(),
            ["show joined", "cancel joined", "show withdrawn", "cancel withdrawn"]
        );
        worker.shutdown();
    }

    struct RetryLog {
        log: Log,
        fails: std::sync::atomic::AtomicBool,
        attempts: std::sync::atomic::AtomicUsize,
    }
    impl NotificationSink for RetryLog {
        fn show(&self, _: &str, _: &str, _: NotificationPriority) -> Result<(), SystemError> {
            Ok(())
        }
        fn show_incoming_call(&self, call: &str, _: &str, _: &str) -> Result<(), SystemError> {
            self.log.0.lock().push(format!("show {call}"));
            Ok(())
        }
        fn cancel_incoming_call(&self, call: &str) -> Result<(), SystemError> {
            self.attempts
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if self.fails.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(SystemError::NotificationSend("fixture failure".into()));
            }
            self.log.0.lock().push(format!("cancel {call}"));
            Ok(())
        }
    }

    #[tokio::test]
    async fn spawned_bridge_retries_watch_accept_cancellation_without_another_event() {
        let worker = worker();
        worker.incoming_presentation_state().begin_session(10);
        let scoped = worker.for_presentation_session(10);
        present(&scoped, "retry-accept").await;
        let log = Arc::new(RetryLog {
            log: Log::default(),
            fails: true.into(),
            attempts: 0.into(),
        });
        let bridge = spawn_session_call_notification_bridge(
            scoped.clone(),
            Arc::new(NotificationService::new(log.clone())),
        );
        wait_for(&log.log, "show retry-accept").await;
        scoped
            .orch_accept_incoming(CallHandle::from("retry-accept"))
            .await
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            while log.attempts.load(std::sync::atomic::Ordering::SeqCst) == 0 {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        log.fails.store(false, std::sync::atomic::Ordering::SeqCst);
        wait_for(&log.log, "cancel retry-accept").await;
        assert!(bridge.stop().await);
        assert_eq!(
            *log.log.0.lock(),
            ["show retry-accept", "cancel retry-accept"]
        );
        worker.shutdown();
    }

    #[tokio::test]
    async fn spawned_bridge_bounded_teardown_retains_failed_cancel_and_retries() {
        let worker = worker();
        worker.incoming_presentation_state().begin_session(11);
        let scoped = worker.for_presentation_session(11);
        present(&scoped, "old-session").await;
        let log = Arc::new(RetryLog {
            log: Log::default(),
            fails: true.into(),
            attempts: 0.into(),
        });
        let bridge = spawn_session_call_notification_bridge(
            scoped,
            Arc::new(NotificationService::new(log.clone())),
        );
        wait_for(&log.log, "show old-session").await;
        let began = std::time::Instant::now();
        assert!(!bridge.stop().await);
        assert!(began.elapsed() < std::time::Duration::from_secs(3));
        let attempts = log.attempts.load(std::sync::atomic::Ordering::SeqCst);
        worker.reset_presentation_session(11).await.unwrap();
        worker.incoming_presentation_state().begin_session(12);
        present(&worker.for_presentation_session(12), "new-session").await;
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        assert!(log.attempts.load(std::sync::atomic::Ordering::SeqCst) > attempts);
        assert_eq!(*log.log.0.lock(), ["show old-session"]);
        log.fails.store(false, std::sync::atomic::Ordering::SeqCst);
        assert!(bridge.stop().await);
        assert_eq!(
            *log.log.0.lock(),
            ["show old-session", "cancel old-session"]
        );
        let next = spawn_session_call_notification_bridge(
            worker.for_presentation_session(12),
            Arc::new(NotificationService::new(log.clone())),
        );
        wait_for(&log.log, "show new-session").await;
        worker
            .for_presentation_session(12)
            .orch_accept_incoming(CallHandle::from("new-session"))
            .await
            .unwrap();
        wait_for(&log.log, "cancel new-session").await;
        assert!(next.stop().await);
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
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
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

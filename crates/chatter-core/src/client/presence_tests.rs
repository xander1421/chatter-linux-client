use super::*;
use crate::domain::ids::UserId;
use std::sync::{
    Mutex as SyncMutex,
    atomic::{AtomicBool, Ordering},
};
use tokio::sync::oneshot;

type Events = Arc<SyncMutex<Vec<&'static str>>>;
struct Dropped(Events, &'static str);
impl Drop for Dropped {
    fn drop(&mut self) {
        self.0.lock().unwrap().push(self.1);
    }
}
fn fixture() -> (Arc<ChatterClient>, tempfile::TempDir, u64) {
    static KEYRING: std::sync::Once = std::sync::Once::new();
    KEYRING
        .call_once(|| keyring::set_default_store(crate::system::keyring::test_credential_store()));
    let dir = tempfile::tempdir().unwrap();
    let client = ChatterClient::builder()
        .server_url("https://localhost:1")
        .data_dir(dir.path().to_str().unwrap())
        .build()
        .unwrap();
    client
        .auth_state()
        .set_logged_in(UserId::new(), 1, "Presence fixture".into());
    let revision = client.auth_state().get().session_revision;
    (client, dir, revision)
}
fn hook(client: &ChatterClient, send: PresenceTestSend) {
    *client.presence_controller.test_send.lock() = Some(send);
}

#[cfg(feature = "media")]
#[tokio::test]
async fn stalled_presence_releases_lifecycle_for_actual_accept_entry() {
    let (client, _dir, revision) = fixture();
    // Establish that this exact Accept entry is fenced by lifecycle (the old
    // presence implementation retained that guard across its network wait).
    let blocker = client.auth_lifecycle_lock.lock().await;
    let mut accept =
        Box::pin(client.accept_presented_incoming_call_in_session("fixture", false, revision));
    assert!(futures::poll!(&mut accept).is_pending());
    drop(blocker);
    assert!(accept.await.is_err()); // No call worker in this offline fixture.

    for stalled_identified in [false, true] {
        let events: Events = Default::default();
        let (entered_tx, entered_rx) = oneshot::channel();
        let entered = Arc::new(SyncMutex::new(Some(entered_tx)));
        let (release_tx, release_rx) = oneshot::channel();
        let release = Arc::new(SyncMutex::new(Some(release_rx)));
        let seen = events.clone();
        hook(
            &client,
            Arc::new(move |_, _| {
                let entered = entered.lock().unwrap().take().unwrap();
                let release = release.lock().unwrap().take().unwrap();
                let events = seen.clone();
                Box::pin(async move {
                    // Model both branches of the production fanout, alternating
                    // which branch stalls. No real audio/network is touched.
                    let slow = async {
                        let _drop = Dropped(events.clone(), "stalled_branch_dropped");
                        entered.send(()).unwrap();
                        release.await.unwrap();
                    };
                    let ready = async {
                        events.lock().unwrap().push("other_branch_completed");
                    };
                    if stalled_identified {
                        tokio::join!(ready, slow);
                    } else {
                        tokio::join!(slow, ready);
                    }
                })
            }),
        );
        let publisher = client.clone();
        let publication = tokio::spawn(async move {
            publisher.set_presence(PresenceStatus::Online).await;
        });
        entered_rx.await.unwrap();
        let accepted = tokio::time::timeout(
            Duration::from_secs(1),
            client.accept_presented_incoming_call_in_session("fixture", false, revision),
        )
        .await;
        assert!(
            accepted.is_ok(),
            "actual Accept must pass lifecycle while presence is pending"
        );
        assert!(
            !publication.is_finished(),
            "presence stays owned, not detached or discarded for Accept"
        );
        assert!(!events.lock().unwrap().contains(&"stalled_branch_dropped"));
        release_tx.send(()).unwrap();
        publication.await.unwrap();
    }
}

#[tokio::test]
async fn logout_cancels_active_and_queued_work_drains_crypto_then_publishes_offline() {
    let (client, _dir, revision) = fixture();
    let events: Events = Default::default();
    let seen = events.clone();
    hook(
        &client,
        Arc::new(move |presence, _| {
            let events = seen.clone();
            Box::pin(async move {
                if presence == PresenceStatus::Online {
                    let _a = Dropped(events.clone(), "anonymous_dropped");
                    let _b = Dropped(events.clone(), "identified_dropped");
                    std::future::pending::<()>().await;
                } else if presence == PresenceStatus::Offline {
                    events.lock().unwrap().push("final_offline");
                } else {
                    panic!("queued publication ran after retirement");
                }
            })
        }),
    );
    let mut active = Box::pin(client.set_presence(PresenceStatus::Online));
    assert!(futures::poll!(&mut active).is_pending());
    let mut queued = Box::pin(client.set_presence_mode_choice("dnd"));
    assert!(futures::poll!(&mut queued).is_pending());
    let mut refresh = Box::pin(client.refresh_identified_presence(revision));
    assert!(futures::poll!(&mut refresh).is_pending());

    let (entered_tx, entered_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    client
        .signal()
        .actor
        .test_pause(entered_tx, release_rx)
        .await;
    entered_rx.await.unwrap();
    let mut logout = Box::pin(client.logout());
    assert!(futures::poll!(&mut logout).is_pending());
    active.await;
    queued.await;
    refresh.await;
    assert!(
        futures::poll!(&mut logout).is_pending(),
        "submitted crypto must drain before Offline"
    );
    assert!(!events.lock().unwrap().contains(&"final_offline"));
    release_tx.send(()).unwrap();
    // No connected auth transport in fixture: local teardown must still finish.
    let _ = tokio::time::timeout(Duration::from_secs(2), &mut logout)
        .await
        .unwrap();
    let events = events.lock().unwrap().clone();
    assert_eq!(events.last(), Some(&"final_offline"));
    assert!(
        events
            .iter()
            .position(|e| *e == "anonymous_dropped")
            .unwrap()
            < events.len() - 1
    );
    assert!(
        events
            .iter()
            .position(|e| *e == "identified_dropped")
            .unwrap()
            < events.len() - 1
    );
    assert!(client.auth_state().get().current_user_id.is_none());
    assert_eq!(
        client.presence_controller.state.lock().await.manual,
        None,
        "queued old-session manual operation must not mutate policy"
    );
}

#[tokio::test]
async fn shutdown_joins_separate_producers_and_fences_retirement_and_same_user_relogin() {
    let (client, _dir, revision) = fixture();
    let user = client.auth_state().get().current_user_id.unwrap();
    let events: Events = Default::default();
    for idle in [true, false] {
        let (entered_tx, entered_rx) = oneshot::channel();
        let seen = events.clone();
        let task = tokio::spawn(async move {
            let _drop = Dropped(seen, if idle { "idle_dropped" } else { "peer_dropped" });
            entered_tx.send(()).unwrap();
            std::future::pending::<()>().await;
        });
        entered_rx.await.unwrap();
        if idle {
            *client.presence_controller.idle_task.lock().await = Some(task);
        } else {
            *client.presence_controller.peer_task.lock().await = Some(task);
        }
    }
    let seen = events.clone();
    hook(
        &client,
        Arc::new(move |status, _| {
            let seen = seen.clone();
            Box::pin(async move {
                assert_eq!(status, PresenceStatus::Offline);
                let mut seen = seen.lock().unwrap();
                assert!(seen.contains(&"idle_dropped") && seen.contains(&"peer_dropped"));
                seen.push("offline");
            })
        }),
    );
    client.shutdown().await;
    let retired = client.auth_state().get().session_revision;
    assert_ne!(revision, retired);
    let ran = AtomicBool::new(false);
    assert!(
        !client
            .with_presence_publication(revision, || async {
                ran.store(true, Ordering::SeqCst);
            })
            .await
    );
    assert!(
        !client
            .with_presence_publication(retired, || async {
                ran.store(true, Ordering::SeqCst);
            })
            .await,
        "invalidated revision still has a user id but must not reopen"
    );
    assert!(!ran.load(Ordering::SeqCst));
    client
        .auth_state()
        .set_logged_in(user, 1, "Same user new session".into());
    let fresh = client.auth_state().get().session_revision;
    assert!(
        client
            .with_presence_publication(fresh, || async {
                ran.store(true, Ordering::SeqCst);
            })
            .await
    );
    assert!(ran.load(Ordering::SeqCst));
    assert!(
        !client
            .with_presence_publication(revision, || async { panic!("stale owner ran") })
            .await
    );
}

#[tokio::test]
async fn manual_choices_and_queued_refresh_preserve_publication_order() {
    let (client, _dir, revision) = fixture();
    let (release_tx, release_rx) = oneshot::channel();
    let release = Arc::new(SyncMutex::new(Some(release_rx)));
    let events = Arc::new(SyncMutex::new(Vec::new()));
    let seen = events.clone();
    hook(
        &client,
        Arc::new(move |status, identified| {
            let release = if status == PresenceStatus::Online {
                release.lock().unwrap().take()
            } else {
                None
            };
            let seen = seen.clone();
            Box::pin(async move {
                seen.lock().unwrap().push((status, identified));
                if let Some(release) = release {
                    release.await.unwrap();
                }
            })
        }),
    );
    let mut online = Box::pin(client.set_presence(PresenceStatus::Online));
    assert!(futures::poll!(&mut online).is_pending());
    let mut dnd = Box::pin(client.set_presence_mode_choice("dnd"));
    assert!(futures::poll!(&mut dnd).is_pending());
    let mut refresh = Box::pin(client.refresh_identified_presence(revision));
    assert!(futures::poll!(&mut refresh).is_pending());
    let mut offline = Box::pin(client.set_presence_mode_choice("offline"));
    assert!(futures::poll!(&mut offline).is_pending());
    release_tx.send(()).unwrap();
    tokio::join!(online, dnd, refresh, offline);
    assert_eq!(
        *events.lock().unwrap(),
        [
            (PresenceStatus::Online, false),
            (PresenceStatus::DoNotDisturb, false),
            (PresenceStatus::DoNotDisturb, true),
            (PresenceStatus::Offline, false),
        ]
    );
    assert_eq!(client.current_presence_status().await, "offline");
    client.refresh_identified_presence(revision).await;
    assert_eq!(
        events.lock().unwrap().len(),
        4,
        "refresh cannot republish an old Online snapshot"
    );
}

#[tokio::test]
async fn failed_or_cancelled_deletion_pause_resumes_only_unchanged_credentials() {
    let (client, _dir, revision) = fixture();
    let lifecycle = client.auth_lifecycle_lock.lock().await;
    let pause = client.suspend_presence(revision).await;
    drop(pause); // rejected deletion
    drop(lifecycle);
    assert!(
        client
            .with_presence_publication(revision, || async {})
            .await
    );

    let mut active =
        Box::pin(client.with_presence_publication(revision, || std::future::pending()));
    assert!(futures::poll!(&mut active).is_pending());
    let lifecycle = client.auth_lifecycle_lock.lock().await;
    let mut pause = Box::pin(client.suspend_presence(revision));
    assert!(futures::poll!(&mut pause).is_pending());
    drop(pause); // caller cancelled while old network work is draining
    assert!(!active.await);
    drop(lifecycle);
    assert!(
        client
            .with_presence_publication(revision, || async {})
            .await
    );

    let lifecycle = client.auth_lifecycle_lock.lock().await;
    let pause = client.suspend_presence(revision).await;
    client.auth().auth_interceptor.clear(); // deletion reached credential clear
    drop(pause);
    drop(lifecycle);
    assert!(
        !client
            .with_presence_publication(revision, || async { panic!("credentials retired") })
            .await
    );
}

#[tokio::test]
async fn committed_deletion_retires_without_offline_and_distinct_user_cannot_reuse_old_work() {
    let (client, _dir, old_revision) = fixture();
    hook(
        &client,
        Arc::new(|_, _| Box::pin(async { panic!("deleted account must not publish Offline") })),
    );
    {
        let _lifecycle = client.auth_lifecycle_lock.lock().await;
        let pause = client.suspend_presence(old_revision).await;
        pause.commit();
        // Successful account service has cleared credentials by this boundary.
        client.auth().auth_interceptor.clear();
        client.clear_composer_session();
        let retired = client.auth_state().get().session_revision;
        client.shutdown_session(retired, false).await;
        client.auth_state().set_logged_out_if_current(retired);
    }
    client
        .auth_state()
        .set_logged_in(UserId::new(), 1, "Different owner".into());
    let new_revision = client.auth_state().get().session_revision;
    assert!(
        !client
            .with_presence_publication(old_revision, || async { panic!("old owner ran") })
            .await
    );
    assert!(
        client
            .with_presence_publication(new_revision, || async {})
            .await
    );
}

#[tokio::test]
async fn cancelled_initialization_recovers_after_temporary_pause_but_not_retirement() {
    use std::sync::atomic::AtomicUsize;
    for queued_before_policy in [false, true] {
        let (client, _dir, revision) = fixture();
        client.set_presence_mode("dnd").unwrap();
        let attempts = Arc::new(AtomicUsize::new(0));
        let seen = attempts.clone();
        hook(
            &client,
            Arc::new(move |_, _| {
                let attempt = seen.fetch_add(1, Ordering::SeqCst);
                Box::pin(async move {
                    if !queued_before_policy && attempt == 0 {
                        std::future::pending::<()>().await;
                    }
                })
            }),
        );
        let mut blocker =
            Box::pin(client.with_presence_publication(revision, || std::future::pending()));
        if queued_before_policy {
            assert!(futures::poll!(&mut blocker).is_pending());
        }
        let mut initialize = Box::pin(client.initialize_presence());
        assert!(futures::poll!(&mut initialize).is_pending());
        let lifecycle = client.auth_lifecycle_lock.lock().await;
        let mut pause = Box::pin(client.suspend_presence(revision));
        assert!(futures::poll!(&mut pause).is_pending());
        if queued_before_policy {
            assert!(!blocker.await);
        }
        assert!(futures::poll!(&mut initialize).is_pending());
        let pause = pause.await;
        drop(pause); // account deletion rejected, same session remains usable
        drop(lifecycle);
        tokio::time::timeout(Duration::from_secs(2), initialize)
            .await
            .unwrap();
        assert_eq!(client.current_presence_status().await, "dnd");
        assert!(client.presence_controller.idle_task.lock().await.is_some());
        assert!(client.presence_controller.peer_task.lock().await.is_some());
        assert!(attempts.load(Ordering::SeqCst) >= if queued_before_policy { 1 } else { 2 });
        let _lifecycle = client.auth_lifecycle_lock.lock().await;
        client.retire_presence().await;
    }

    let (client, _dir, _revision) = fixture();
    hook(&client, Arc::new(|_, _| Box::pin(std::future::pending())));
    let mut initialize = Box::pin(client.initialize_presence());
    assert!(futures::poll!(&mut initialize).is_pending());
    let lifecycle = client.auth_lifecycle_lock.lock().await;
    let mut retire = Box::pin(client.retire_presence());
    assert!(futures::poll!(&mut retire).is_pending());
    assert!(futures::poll!(&mut initialize).is_pending());
    retire.await;
    drop(lifecycle);
    initialize.await;
    assert!(client.presence_controller.idle_task.lock().await.is_none());
    assert!(client.presence_controller.peer_task.lock().await.is_none());
}

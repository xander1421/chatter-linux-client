use super::*;

#[test]
fn bootstrap_targets_every_device_newest_first() {
    assert_eq!(bootstrap_device_order(vec![1, 6, 3, 6]), vec![6, 3, 1]);
    assert_eq!(bootstrap_device_order(vec![2]), vec![2]);
    assert!(bootstrap_device_order(vec![]).is_empty());
}

/// Shield, first contact, nothing cached: a clear refusal — no
/// identified fetch, no device-1 guess, no mailbox made, nothing queued.
#[tokio::test]
async fn shield_bootstrap_without_cached_devices_is_not_reachable_privately() {
    // Lower UUID: we would initiate (make a mailbox, spend a token).
    let me = UserId::from_uuid(uuid::Uuid::from_u128(1));
    let friend = UserId::from_uuid(uuid::Uuid::max());
    let (client, _dir) = shield_client(me);
    let ms = client.messages();

    let sent = ms
        .send_friend_dm_content(&friend, crate::services::build_text_content("hi", 1))
        .await;
    assert!(
        matches!(sent, Err(MessageError::NotReachablePrivately(p)) if p == friend),
        "got {sent:?}"
    );
    assert!(ms.pending_dms.drain(&friend).unwrap().is_empty());
    assert!(
        crate::mailbox::dm::DmConversationManager::new(ms.database.clone())
            .load_conversation(&friend)
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn public_core_shield_refusal_releases_admission_for_later_reachable_preparation() {
    use crate::state::ActiveConversation;
    let me = UserId::from_uuid(uuid::Uuid::from_u128(1));
    let friend = UserId::from_uuid(uuid::Uuid::max());
    let (client, _dir) = shield_client(me);
    client.auth_state().set_logged_in(me, 1, "me".into());
    let conversation = ActiveConversation::ServerDm { peer_id: friend };
    let refused = client.send_text(conversation, "first").await.unwrap();
    assert!(!refused.bootstrap_initiated);
    assert!(refused.send_error.is_some());
    assert_eq!(refused.message.unwrap().delivery_status, "failed");
    client.ensure_send_available(conversation).unwrap();
    assert!(
        client
            .messages()
            .pending_dms
            .drain(&friend)
            .unwrap()
            .is_empty()
    );

    cache_device(
        client.messages(),
        &friend,
        1,
        &mut StdRng::seed_from_u64(19),
    )
    .await;
    let mut auth = client.messages().auth_client();
    assert_eq!(
        client
            .messages()
            .bootstrap_devices(&mut auth, &friend)
            .await
            .unwrap(),
        vec![1]
    );
    seed_tokens(client.messages(), 2);
    // A later public send now reaches actual mailbox bootstrap. The offline
    // endpoint fails after its boundary, so admission must stay uncertain.
    let later = client.send_text(conversation, "second").await.unwrap();
    assert_eq!(later.message.unwrap().delivery_status, "sending");
    assert!(client.ensure_send_available(conversation).is_err());
}

/// Shield uses every cached device, newest first, skipping one the server
/// has refused as retired — all without a single RPC.
#[tokio::test]
async fn shield_bootstrap_targets_cached_devices_newest_first() {
    let me = UserId::from_uuid(uuid::Uuid::from_u128(1));
    let friend = UserId::from_uuid(uuid::Uuid::max());
    let (client, _dir) = shield_client(me);
    let ms = client.messages();
    let mut rng = StdRng::seed_from_u64(0xF34);
    for device in [1, 6, 3] {
        cache_device(ms, &friend, device, &mut rng).await;
    }

    let mut auth = ms.auth_client();
    assert_eq!(
        ms.bootstrap_devices(&mut auth, &friend).await.unwrap(),
        vec![6, 3, 1]
    );

    ms.retired_peer_devices
        .lock()
        .insert((friend, 6), std::time::Instant::now());
    assert_eq!(
        ms.bootstrap_devices(&mut auth, &friend).await.unwrap(),
        vec![3, 1]
    );
}

/// Cache a sender certificate for `me` (device 1) that chains to the dev
/// trust root this test build pins — its private key is committed in
/// `deploy/vault/init-and-unseal.sh` — so sealing needs no RPC.
/// F39: the responder's post-bootstrap flush sealed every queued message
/// to device 1 — a guess, and an identified prekey fetch naming the peer
/// when device 1 had no session. With the peer on device 6 only, the flush
/// now seals to 6 with no RPC at all (nothing is behind the server URL, so
/// any fetch would fail the call).
#[tokio::test]
async fn shield_flush_seals_to_the_session_device_without_a_prekey_fetch() {
    let me = UserId::from_uuid(uuid::Uuid::max());
    let friend = UserId::from_uuid(uuid::Uuid::from_u128(1));
    let (client, _dir) = shield_client(me);
    let ms = client.messages();
    cache_sender_certificate(ms, &me);
    cache_device(ms, &friend, 6, &mut StdRng::seed_from_u64(6)).await;

    let mut auth = ms.auth_client();
    let sealed = crate::inbox::streaming::dm_bootstrap_handler::seal_for_peer_mailbox(
        &ms.signal_service,
        &mut auth,
        &friend,
        6,
        &crate::network::validation::protovalidate::test_plaintext("queued"),
        &[7u8; 32],
    )
    .await
    .expect("sealed without any RPC");
    assert_eq!(sealed.len(), 1, "one envelope, for device 6");
}

/// A send spends only the pre-fetched batch: with it empty, the send fails
/// on the empty batch itself — not on a network error from a token
/// request made in the same call — and makes no mailbox.
#[tokio::test]
async fn shield_send_with_empty_token_batch_fails_without_requesting_tokens() {
    let me = UserId::from_uuid(uuid::Uuid::from_u128(1));
    let friend = UserId::from_uuid(uuid::Uuid::max());
    let (client, _dir) = shield_client(me);
    let ms = client.messages();
    cache_device(ms, &friend, 2, &mut StdRng::seed_from_u64(9)).await;

    let sent = ms
        .send_friend_dm_content(&friend, crate::services::build_text_content("hi", 1))
        .await;
    match sent {
        Err(MessageError::ShieldNotReady) => {}
        other => panic!("expected the empty-batch refusal, got {other:?}"),
    }
    assert_eq!(ms.token_manager.available_token_count().unwrap(), 0);
    assert!(ms.pending_dms.drain(&friend).unwrap().is_empty());
}

/// The composing path must retain an empty-batch refusal as a failed,
/// retryable bubble, rather than reporting it sent or losing the content.
#[tokio::test]
async fn shield_empty_token_send_keeps_a_failed_bubble_for_retry() {
    let me = UserId::from_uuid(uuid::Uuid::from_u128(1));
    let friend = UserId::from_uuid(uuid::Uuid::max());
    let (client, _dir) = shield_client(me);
    let ms = client.messages();
    cache_device(ms, &friend, 2, &mut StdRng::seed_from_u64(9)).await;

    let result = ms
        .send_server_dm(&friend, "keep this", vec![], 12345, None)
        .await
        .expect("local echo persists even when delivery fails");
    assert!(matches!(
        result.send_error,
        Some(MessageError::ShieldNotReady)
    ));
    assert!(!result.bootstrap_initiated);
    let row = result.message.expect("outgoing bubble");
    let stored = ms.message_repo.get(&row.id).unwrap().unwrap();
    assert_eq!(stored.delivery_status, "failed");
    assert_eq!(
        stored.failure_reason.as_deref(),
        Some(super::super::core::SHIELD_WAITING_FOR_TOKENS)
    );
    assert_eq!(stored.as_text(), Some("keep this"));
    assert_eq!(ms.token_manager.available_token_count().unwrap(), 0);

    // Retry must keep the original row and its failed status if the pool
    // is still empty. No token request may be coupled to either attempt.
    assert!(ms.retry_server_dm(&row.id).await.is_err());
    let retried = ms.message_repo.get(&row.id).unwrap().unwrap();
    assert_eq!(retried.delivery_status, "failed");
    assert_eq!(retried.sent_at, stored.sent_at);
    assert_eq!(retried.as_text(), Some("keep this"));
}

/// Deterministic retry boundary: the real composing path parks locally;
/// a simulated scheduled refill does not wake a send, and the independent
/// retry tick submits the original row once. The callback stands for the
/// transport owner (production uses the client's durable retry journal).
#[tokio::test(start_paused = true)]
async fn shield_parked_post_waits_for_independent_retry_after_refill() {
    let me = UserId::from_uuid(uuid::Uuid::from_u128(1));
    let friend = UserId::from_uuid(uuid::Uuid::max());
    let (client, _dir) = shield_client(me);
    client.auth_state().set_logged_in(me, 1, "me".into());
    let conversation = crate::state::ActiveConversation::ServerDm { peer_id: friend };
    let ms = client.messages();
    cache_device(ms, &friend, 2, &mut StdRng::seed_from_u64(97)).await;
    let result = client
        .send_text(conversation, "park exactly once")
        .await
        .unwrap();
    client.ensure_send_available(conversation).unwrap();
    assert!(matches!(
        result.send_error,
        Some(MessageError::ShieldNotReady)
    ));
    let row = result.message.unwrap();
    assert_eq!(
        super::super::display_delivery_status(&row.delivery_status, row.failure_reason.as_deref()),
        "parked"
    );
    assert!(ms.pending_dms.drain(&friend).unwrap().is_empty());
    assert_eq!(
        super::super::shield_retry::retry_parked_with(ms, me, |_| async {
            panic!("empty pool must not dispatch")
        })
        .await
        .unwrap(),
        0
    );
    assert!(matches!(
        client.retry_failed_message(&row.id.to_string()).await,
        Err(crate::services::gateway::outgoing::SendError::Message(
            MessageError::ShieldNotReady
        ))
    ));

    client.ensure_send_available(conversation).unwrap();
    // Start the independent retry timer before simulating a refill.
    let mut retry_tick = Box::pin(crate::app::core_services::jittered_sleep());
    assert!(futures::poll!(&mut retry_tick).is_pending());
    seed_tokens(ms, 2);
    assert_eq!(
        ms.message_repo
            .get(&row.id)
            .unwrap()
            .unwrap()
            .delivery_status,
        "failed"
    );
    tokio::time::advance(std::time::Duration::from_secs(299)).await;
    assert!(
        futures::poll!(&mut retry_tick).is_pending(),
        "refill cannot wake the retry timer"
    );
    tokio::time::advance(std::time::Duration::from_secs(601)).await;
    retry_tick.await;
    let delivered = std::sync::atomic::AtomicUsize::new(0);
    let attempt = |id| {
        assert_eq!(id, row.id);
        assert_eq!(
            ms.message_repo.get(&id).unwrap().unwrap().as_text(),
            Some("park exactly once")
        );
        delivered.fetch_add(1, Ordering::SeqCst);
        ms.finish_retry_row(&id, "sent", None).unwrap();
        std::future::ready(())
    };
    assert_eq!(
        super::super::shield_retry::retry_parked_with(ms, me, attempt)
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        super::super::shield_retry::retry_parked_with(ms, me, |_| async {
            panic!("confirmed row must not repeat")
        })
        .await
        .unwrap(),
        0
    );
    assert_eq!(delivered.load(Ordering::SeqCst), 1);
    assert_eq!(
        ms.token_manager.available_token_count().unwrap(),
        2,
        "tick does not request or claim tokens; the transport owner spends them"
    );
}

use super::*;

/// F54: a REQUEST goes to EVERY live device of a multi-device peer, each
/// sealed to that device under its own token — not to the newest one
/// only (the offline Tab while the S9 was online). Nothing is behind the
/// server URL, so the batched deposit fails after its credential is spent:
/// four independently encrypted devices, one batch credential.
#[tokio::test]
async fn request_fans_out_to_every_live_device() {
    let me = UserId::from_uuid(uuid::Uuid::max());
    let friend = UserId::from_uuid(uuid::Uuid::from_u128(1));
    let (client, _dir) = shield_client(me);
    let ms = client.messages();
    cache_sender_certificate(ms, &me);
    let mut rng = StdRng::seed_from_u64(0xF54);
    for device in [2, 6, 7, 8] {
        cache_device(ms, &friend, device, &mut rng).await;
    }
    seed_half_open(ms, &friend, &crate::mailbox::dm::ConversationKey::new(), 5);
    seed_tokens(ms, 10);

    let before: Vec<_> = [2, 6, 7, 8]
        .into_iter()
        .map(|device| {
            ms.signal_service
                .key_store
                .load_session(&format!("{friend}:{device}"))
                .unwrap()
                .expect("cached device session")
        })
        .collect();

    let sent = ms
        .send_friend_dm_content(&friend, crate::services::build_text_content("hi", 1))
        .await;
    assert!(
        matches!(sent, Err(MessageError::DmError(ref m)) if m.contains("Failed to send bootstrap")),
        "got {sent:?}"
    );
    assert_eq!(
        ms.token_manager.available_token_count().unwrap(),
        9,
        "one credential covers the complete device batch"
    );
    for (device, original) in [2, 6, 7, 8].into_iter().zip(before) {
        let after = ms
            .signal_service
            .key_store
            .load_session(&format!("{friend}:{device}"))
            .unwrap()
            .expect("device session retained");
        assert_ne!(after, original, "REQUEST encrypted for device {device}");
    }
}

/// F54: with device 6 ready, a send owes its copy to devices 7 and 8
/// (no half yet) and asks each of them. An ask that did not get through
/// (nothing is behind the server URL) is repeated on the next send; one
/// that did is not. Device 6, which has the message, is owed nothing.
#[tokio::test]
async fn unready_devices_are_owed_each_copy_and_asked_once() {
    use crate::mailbox::dm::{ConversationKey, DmConversationManager};
    let me = UserId::from_uuid(uuid::Uuid::max());
    let friend = UserId::from_uuid(uuid::Uuid::from_u128(1));
    let (client, _dir) = shield_client(me);
    let ms = client.messages();
    cache_sender_certificate(ms, &me);
    let mut rng = StdRng::seed_from_u64(0xF55);
    for device in [6, 7, 8] {
        cache_device(ms, &friend, device, &mut rng).await;
    }
    seed_half_open(ms, &friend, &ConversationKey::new(), 5);
    let conversations = DmConversationManager::new(ms.database.clone());
    conversations
        .record_peer_device(
            &friend,
            6,
            [6u8; 32],
            ConversationKey::new().our_ephemeral_public,
        )
        .unwrap();
    let conv = conversations.load_conversation(&friend).unwrap().unwrap();
    assert!(conv.is_ready_to_send());
    seed_tokens(ms, 10);

    let mut auth = ms.auth_client();
    ms.owe_unready_devices(&mut auth, &friend, &conv, b"one", Some(&ms.grpc_pool))
        .await;
    assert_eq!(
        ms.token_manager.available_token_count().unwrap(),
        8,
        "7 and 8 asked"
    );
    ms.owe_unready_devices(&mut auth, &friend, &conv, b"two", Some(&ms.grpc_pool))
        .await;
    assert_eq!(
        ms.token_manager.available_token_count().unwrap(),
        6,
        "failed asks repeated"
    );
    for device in [7, 8] {
        conversations.mark_device_asked(&friend, device).unwrap();
    }
    ms.owe_unready_devices(&mut auth, &friend, &conv, b"three", Some(&ms.grpc_pool))
        .await;
    assert_eq!(
        ms.token_manager.available_token_count().unwrap(),
        6,
        "a delivered ask is not repeated"
    );

    assert!(
        conversations
            .drain_device_queue(&friend, 6)
            .unwrap()
            .is_empty()
    );
    for device in [7, 8] {
        assert_eq!(
            conversations.drain_device_queue(&friend, device).unwrap(),
            vec![b"one".to_vec(), b"two".to_vec(), b"three".to_vec()]
        );
    }
}

/// F55: a device that answers after another already took the Sesame queue
/// finds that queue empty; what it is owed (queued when the first device
/// got the copies) still goes to it. The responder flush used to return on
/// the empty queue, so the late Tab never got rc4–rc6. Nothing is behind
/// the server URL: the deposit fails, but the owed copy was taken for it.
#[tokio::test]
async fn late_device_gets_what_it_is_owed_when_nothing_is_queued() {
    use crate::mailbox::dm::{ConversationKey, DmConversationManager};
    let me = UserId::from_uuid(uuid::Uuid::max());
    let friend = UserId::from_uuid(uuid::Uuid::from_u128(1));
    let (client, _dir) = shield_client(me);
    let ms = client.messages();
    cache_sender_certificate(ms, &me);
    let mut rng = StdRng::seed_from_u64(0xF55B);
    for device in [6, 8] {
        cache_device(ms, &friend, device, &mut rng).await;
    }
    seed_half_open(ms, &friend, &ConversationKey::new(), 5);
    let conversations = DmConversationManager::new(ms.database.clone());
    conversations.queue_for_device(&friend, 8, b"rc4").unwrap();
    conversations
        .record_peer_device(
            &friend,
            8,
            [8u8; 32],
            ConversationKey::new().our_ephemeral_public,
        )
        .unwrap();
    assert!(ms.pending_dms.drain(&friend).unwrap().is_empty());

    let mut auth = ms.auth_client();
    let flush =
        crate::inbox::streaming::dm_bootstrap_handler::send_pending_messages_after_bootstrap(
            friend,
            8,
            ms.database.clone(),
            &ms.signal_service,
            &mut auth,
            ms.grpc_pool.unidentified_channel(),
            ms,
        )
        .await;
    assert!(flush.is_none(), "nothing queued, nothing to announce");
    assert!(
        conversations
            .drain_device_queue(&friend, 8)
            .unwrap()
            .is_empty(),
        "owed copy taken for device 8"
    );
}

/// A DM queued before the Shield bootstrap finished and sent by the
/// responder's flush records its receipt targets like any send: the
/// peer device's Receipt naming the original DataMessage timestamp makes
/// the bubble "delivered". The flush used to deposit without recording
/// them, so that Receipt matched nothing and the bubble stayed "sent".
/// Nothing is behind the server URL: the deposit fails, but the targets
/// are captured before transport, as `send_dm_content` does.
#[tokio::test]
async fn flushed_pending_dm_is_marked_delivered_by_the_peer_receipt() {
    use crate::domain::ids::{ChannelId, DeviceId as Device};
    use crate::domain::message::{ConversationType, Message};
    use crate::mailbox::dm::{ConversationKey, DmConversationManager};
    use prost::Message as _;
    let me = UserId::from_uuid(uuid::Uuid::max());
    let friend = UserId::from_uuid(uuid::Uuid::from_u128(1));
    let (client, _dir) = shield_client(me);
    let ms = client.messages();
    cache_sender_certificate(ms, &me);
    cache_device(ms, &friend, 6, &mut StdRng::seed_from_u64(0x5EC)).await;
    seed_half_open(ms, &friend, &ConversationKey::new(), 5);

    // Queued before the bootstrap, stored as a "sending" bubble.
    let sent_at = 1_700_000_000_123_456_789u64;
    let content = crate::services::build_text_content("first", sent_at);
    ms.pending_dms
        .queue(&friend, content.encode_to_vec(), vec![], None)
        .unwrap();
    let mut message = Message::new_text(
        ChannelId::from(friend),
        ConversationType::ServerDm,
        Some(me),
        "first".into(),
        sent_at,
    );
    message.delivery_status = "sending".into();
    ms.message_repo.insert_outgoing(&message).unwrap();

    // The peer's device 6 answers the bootstrap; the queue is flushed.
    DmConversationManager::new(ms.database.clone())
        .record_peer_device(
            &friend,
            6,
            [6u8; 32],
            ConversationKey::new().our_ephemeral_public,
        )
        .unwrap();
    let mut auth = ms.auth_client();
    crate::inbox::streaming::dm_bootstrap_handler::send_pending_messages_after_bootstrap(
        friend,
        6,
        ms.database.clone(),
        &ms.signal_service,
        &mut auth,
        ms.grpc_pool.unidentified_channel(),
        ms,
    )
    .await
    .expect("one message was queued");

    // Device 6's delivery Receipt names the original timestamp.
    ms.apply_peer_receipt(
        ChannelId::from(friend),
        ConversationType::ServerDm,
        friend,
        Device(6),
        1,
        &[sent_at],
    )
    .unwrap();
    let stored = ms.message_repo.get(&message.id).unwrap().unwrap();
    assert_eq!(stored.delivery_status, "delivered");
}

/// Initiator side (S25, 2026-10-03): a DM queued before the bootstrap and
/// sent by `send_pending_messages` is stored "delivered" by the peer
/// device's Receipt, and the flush's later `DmBootstrapCompleted` "sent"
/// write does not undo it. The stored state was right on the phone too;
/// the bubble missed the update because it carried a synthetic id
/// (`client::queued_bubble_id`).
#[tokio::test]
async fn initiator_flush_completion_does_not_regress_a_delivered_pending_dm() {
    use crate::domain::ids::{ChannelId, DeviceId as Device};
    use crate::domain::message::{ConversationType, Message};
    use crate::mailbox::dm::{ConversationKey, DmConversationManager};
    use prost::Message as _;
    let me = UserId::from_uuid(uuid::Uuid::from_u128(1));
    let friend = UserId::from_uuid(uuid::Uuid::max());
    let (client, _dir) = shield_client(me);
    let ms = client.messages();
    cache_sender_certificate(ms, &me);
    cache_device(ms, &friend, 6, &mut StdRng::seed_from_u64(0x1417)).await;
    seed_half_open(ms, &friend, &ConversationKey::new(), 5);

    let sent_at = 1_700_000_000_987_654_321u64;
    let content = crate::services::build_text_content("shield over tor", sent_at);
    ms.pending_dms
        .queue(&friend, content.encode_to_vec(), vec![], None)
        .unwrap();
    let mut message = Message::new_text(
        ChannelId::from(friend),
        ConversationType::ServerDm,
        Some(me),
        "shield over tor".into(),
        sent_at,
    );
    message.delivery_status = "sending".into();
    ms.message_repo.insert_outgoing(&message).unwrap();

    // The RESPONSE from device 6 arrived; the initiator flushes.
    DmConversationManager::new(ms.database.clone())
        .record_peer_device(
            &friend,
            6,
            [6u8; 32],
            ConversationKey::new().our_ephemeral_public,
        )
        .unwrap();
    let mut auth = ms.auth_client();
    let _ = ms
        .send_pending_messages(&mut auth, &friend, &ms.mailbox_manager)
        .await;

    ms.apply_peer_receipt(
        ChannelId::from(friend),
        ConversationType::ServerDm,
        friend,
        Device(6),
        1,
        &[sent_at],
    )
    .unwrap();
    assert_eq!(
        ms.message_repo
            .get(&message.id)
            .unwrap()
            .unwrap()
            .delivery_status,
        "delivered"
    );

    // What the `DmBootstrapCompleted` handler then persists.
    ms.set_friend_dm_delivery_status(&ChannelId::from(friend), &[sent_at], "sent")
        .unwrap();
    assert_eq!(
        ms.message_repo
            .get(&message.id)
            .unwrap()
            .unwrap()
            .delivery_status,
        "delivered"
    );
}

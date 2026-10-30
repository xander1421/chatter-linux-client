//! Durable receipt evidence, scoped by message, recipient and authenticated device.
use chatter_core::domain::ids::{ChannelId, DeviceId, UserId};
use chatter_core::domain::message::{ConversationType, Message};
use chatter_core::storage::repositories::message_receipt_repo::MessageReceiptRepository;
use chatter_core::storage::{Database, MessageRepository};
use std::sync::Arc;

#[test]
fn receipt_aggregation_survives_reopen_and_never_regresses() {
    let temp = tempfile::TempDir::new().unwrap();
    let path = temp.path().join("receipts.db");
    let author = UserId::new();
    let alice = UserId::new();
    let bob = UserId::new();
    let channel = ChannelId::new();
    let id;
    {
        let db = Arc::new(Database::open(&path, "test").unwrap());
        db.run_migrations().unwrap();
        let messages = MessageRepository::new(db.clone());
        let receipts = MessageReceiptRepository::new(db);
        let mut message = Message::new_text(
            channel,
            ConversationType::Channel,
            Some(author),
            "hi".into(),
            123,
        );
        message.delivery_status = "sent".into();
        messages.insert_outgoing(&message).unwrap();
        id = message.id;
        receipts
            .snapshot_targets(
                id,
                author,
                &[
                    (alice, DeviceId(1)),
                    (alice, DeviceId(2)),
                    (bob, DeviceId(1)),
                ],
            )
            .unwrap();
        let apply = |user, device, read| {
            receipts
                .apply(
                    channel,
                    ConversationType::Channel,
                    author,
                    user,
                    DeviceId(device),
                    &[123],
                    read,
                    456,
                )
                .unwrap()
        };
        assert!(apply(UserId::new(), 1, true).is_empty());
        assert!(apply(alice, 9, true).is_empty());
        apply(alice, 1, true);
        assert_eq!(messages.get(&id).unwrap().unwrap().delivery_status, "sent");
        apply(bob, 1, false);
        assert_eq!(
            messages.get(&id).unwrap().unwrap().delivery_status,
            "delivered"
        );
        apply(bob, 1, true);
        assert_eq!(messages.get(&id).unwrap().unwrap().delivery_status, "read");
        // Alice's offline second device does not hold her recipient state back.
        assert_eq!(
            receipts
                .for_message(id)
                .unwrap()
                .iter()
                .filter(|r| r.read_at.is_some())
                .count(),
            2
        );
        apply(bob, 1, false);
        messages
            .update_delivery_status(&channel, &ConversationType::Channel, 123, "sent")
            .unwrap();
        message.delivery_status = "sending".into();
        messages.update(&message).unwrap();
        assert_eq!(messages.get(&id).unwrap().unwrap().delivery_status, "read");
        // Membership changes or retries cannot change the historical recipient set.
        receipts
            .snapshot_targets(id, author, &[(UserId::new(), DeviceId(1))])
            .unwrap();
        assert_eq!(receipts.for_message(id).unwrap().len(), 3);
    }
    let db = Arc::new(Database::open(&path, "test").unwrap());
    let messages = MessageRepository::new(db.clone());
    let receipts = MessageReceiptRepository::new(db);
    assert_eq!(messages.get(&id).unwrap().unwrap().delivery_status, "read");
    assert_eq!(receipts.for_message(id).unwrap().len(), 3);
    messages.delete(&id).unwrap();
    assert!(receipts.for_message(id).unwrap().is_empty());
}

#[test]
fn receipt_cannot_cross_conversation_author_or_timestamp() {
    let temp = tempfile::TempDir::new().unwrap();
    let db = Arc::new(Database::open(&temp.path().join("receipts.db"), "test").unwrap());
    db.run_migrations().unwrap();
    let messages = MessageRepository::new(db.clone());
    let receipts = MessageReceiptRepository::new(db);
    let author = UserId::new();
    let peer = UserId::new();
    let first = ChannelId::from(peer);
    let second = ChannelId::new();
    let mut ids = Vec::new();
    for channel in [first, second] {
        let mut message = Message::new_text(
            channel,
            ConversationType::ServerDm,
            Some(author),
            "hi".into(),
            123,
        );
        message.delivery_status = "sent".into();
        messages.insert_outgoing(&message).unwrap();
        receipts
            .snapshot_targets(message.id, author, &[(peer, DeviceId(1))])
            .unwrap();
        ids.push(message.id);
    }
    assert!(
        receipts
            .apply(
                first,
                ConversationType::ServerDm,
                peer,
                peer,
                DeviceId(1),
                &[123],
                true,
                456
            )
            .unwrap()
            .is_empty()
    );
    assert!(
        receipts
            .apply(
                first,
                ConversationType::ServerDm,
                author,
                peer,
                DeviceId(1),
                &[124],
                true,
                456
            )
            .unwrap()
            .is_empty()
    );
    receipts
        .apply(
            first,
            ConversationType::ServerDm,
            author,
            peer,
            DeviceId(1),
            &[123],
            true,
            456,
        )
        .unwrap();
    assert_eq!(
        messages.get(&ids[0]).unwrap().unwrap().delivery_status,
        "read"
    );
    assert_eq!(
        messages.get(&ids[1]).unwrap().unwrap().delivery_status,
        "sent"
    );
}

#[test]
fn receipt_before_local_echo_is_attached_on_insert() {
    let temp = tempfile::TempDir::new().unwrap();
    let db = Arc::new(Database::open(&temp.path().join("early.db"), "test").unwrap());
    db.run_migrations().unwrap();
    let messages = MessageRepository::new(db.clone());
    let receipts = MessageReceiptRepository::new(db);
    let author = UserId::new();
    let peer = UserId::new();
    let channel = ChannelId::from(peer);
    receipts
        .snapshot_for_send(
            channel,
            ConversationType::ServerDm,
            author,
            123,
            &[(peer, DeviceId(1))],
        )
        .unwrap();
    assert!(
        receipts
            .apply(
                channel,
                ConversationType::ServerDm,
                author,
                peer,
                DeviceId(1),
                &[123],
                true,
                456
            )
            .unwrap()
            .is_empty()
    );
    let message = Message::new_text(
        channel,
        ConversationType::ServerDm,
        Some(author),
        "hi".into(),
        123,
    );
    messages.insert_outgoing(&message).unwrap();
    assert_eq!(
        messages.get(&message.id).unwrap().unwrap().delivery_status,
        "read"
    );
    assert_eq!(
        receipts.for_message(message.id).unwrap()[0].read_at,
        Some(456)
    );
}

#[test]
fn public_channel_receipt_waits_for_server_timestamp_binding() {
    for echo_first in [true, false] {
        let temp = tempfile::TempDir::new().unwrap();
        let db = Arc::new(Database::open(&temp.path().join("public.db"), "test").unwrap());
        db.run_migrations().unwrap();
        let messages = MessageRepository::new(db.clone());
        let receipts = MessageReceiptRepository::new(db);
        let author = UserId::new();
        let peer = UserId::new();
        let channel = ChannelId::new();
        receipts
            .snapshot_for_send(
                channel,
                ConversationType::Channel,
                author,
                123,
                &[(peer, DeviceId(1))],
            )
            .unwrap();
        let message = Message::new_text(
            channel,
            ConversationType::Channel,
            Some(author),
            "hi".into(),
            200,
        );
        if echo_first {
            messages.insert_outgoing(&message).unwrap();
        }
        receipts
            .apply(
                channel,
                ConversationType::Channel,
                author,
                peer,
                DeviceId(1),
                &[200],
                true,
                456,
            )
            .unwrap();
        receipts
            .bind_send_timestamp(channel, ConversationType::Channel, author, 123, 200)
            .unwrap();
        if !echo_first {
            messages.insert_outgoing(&message).unwrap();
        }
        assert_eq!(
            messages.get(&message.id).unwrap().unwrap().delivery_status,
            "read"
        );
        assert_eq!(
            receipts.for_message(message.id).unwrap()[0].read_at,
            Some(456)
        );
    }
}

#[test]
fn phone_send_and_linked_tablet_receipts_converge_in_either_order() {
    use chatter_core::proto::content_v1::{
        DataMessage, SyncSent, SyncSentReceiptTarget, sync_sent::Destination,
    };
    use prost::Message as _;

    for receipt_first in [false, true] {
        let temp = tempfile::TempDir::new().unwrap();
        let phone = Arc::new(Database::open(&temp.path().join("phone.db"), "test").unwrap());
        let tablet = Arc::new(Database::open(&temp.path().join("tablet.db"), "test").unwrap());
        phone.run_migrations().unwrap();
        tablet.run_migrations().unwrap();
        let author = UserId::new();
        let peer = UserId::new();
        let channel = ChannelId::from(peer);
        let kind = ConversationType::ServerDm;
        let sent_at = 123;
        let phone_receipts = MessageReceiptRepository::new(phone.clone());
        let tablet_receipts = MessageReceiptRepository::new(tablet.clone());
        phone_receipts
            .snapshot_for_send(
                channel,
                kind.clone(),
                author,
                sent_at,
                &[(peer, DeviceId(1)), (peer, DeviceId(2))],
            )
            .unwrap();
        let sync = SyncSent {
            destination: Some(Destination::DestinationUserId(peer.to_string())),
            timestamp: sent_at,
            message: Some(DataMessage {
                body: "sent from phone".into(),
                timestamp: sent_at,
                ..Default::default()
            }),
            receipt_targets: phone_receipts
                .targets_for_send(channel, kind.clone(), author, sent_at)
                .unwrap()
                .into_iter()
                .map(|(user, device)| SyncSentReceiptTarget {
                    user_id: user.to_string(),
                    device_id: device.0,
                })
                .collect(),
        };
        let wire = SyncSent::decode(sync.encode_to_vec().as_slice()).unwrap();
        let targets: Vec<_> = wire
            .receipt_targets
            .iter()
            .map(|target| (target.user_id.parse().unwrap(), DeviceId(target.device_id)))
            .collect();
        let apply = |receipts: &MessageReceiptRepository, device, read| {
            receipts
                .apply(
                    channel,
                    kind.clone(),
                    author,
                    peer,
                    DeviceId(device),
                    &[sent_at],
                    read,
                    456,
                )
                .unwrap();
        };
        if receipt_first {
            apply(&tablet_receipts, 2, true);
            // A peer device outside the eventual snapshot cannot add evidence.
            apply(&tablet_receipts, 99, true);
        }
        tablet_receipts
            .snapshot_for_send(channel, kind.clone(), author, sent_at, &targets)
            .unwrap();
        let phone_message = Message::new_text(
            channel,
            kind.clone(),
            Some(author),
            "sent from phone".into(),
            sent_at,
        );
        let tablet_message = Message::new_text(
            channel,
            kind.clone(),
            Some(author),
            wire.message.unwrap().body,
            sent_at,
        );
        let phone_messages = MessageRepository::new(phone);
        let tablet_messages = MessageRepository::new(tablet);
        phone_messages.insert_outgoing(&phone_message).unwrap();
        tablet_messages.insert_outgoing(&tablet_message).unwrap();
        apply(&phone_receipts, 2, true);
        if !receipt_first {
            apply(&tablet_receipts, 2, true);
        }
        apply(&tablet_receipts, 1, false);
        assert_eq!(
            phone_messages
                .get(&phone_message.id)
                .unwrap()
                .unwrap()
                .delivery_status,
            "read"
        );
        assert_eq!(
            tablet_messages
                .get(&tablet_message.id)
                .unwrap()
                .unwrap()
                .delivery_status,
            "read"
        );
        let rows = tablet_receipts.for_message(tablet_message.id).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1].read_at, Some(456));
    }
}

#[test]
fn receipt_before_sync_cannot_escape_the_later_snapshot() {
    let temp = tempfile::TempDir::new().unwrap();
    let db = Arc::new(Database::open(&temp.path().join("scoped.db"), "test").unwrap());
    db.run_migrations().unwrap();
    let receipts = MessageReceiptRepository::new(db.clone());
    let messages = MessageRepository::new(db);
    let author = UserId::new();
    let peer = UserId::new();
    let channel = ChannelId::from(peer);
    receipts
        .apply(
            channel,
            ConversationType::ServerDm,
            author,
            peer,
            DeviceId(99),
            &[123],
            true,
            456,
        )
        .unwrap();
    receipts
        .snapshot_for_send(
            channel,
            ConversationType::ServerDm,
            author,
            123,
            &[(peer, DeviceId(1))],
        )
        .unwrap();
    let mut message = Message::new_text(
        channel,
        ConversationType::ServerDm,
        Some(author),
        "hi".into(),
        123,
    );
    message.delivery_status = "sent".into();
    messages.insert_outgoing(&message).unwrap();
    assert_eq!(
        messages.get(&message.id).unwrap().unwrap().delivery_status,
        "sent"
    );
    assert_eq!(receipts.for_message(message.id).unwrap()[0].read_at, None);
}

use super::account_keys::{conversation_for_notification_key, notification_key};
use super::listener::{is_private_notification, should_notify_message};
use super::*;

#[test]
fn own_message_never_notifies() {
    use crate::domain::ids::{ChannelId, UserId};
    use crate::domain::message::{ConversationType, Message};

    let me = UserId::new();
    let peer = UserId::new();
    let msg = |sender| {
        Message::new_text(
            ChannelId::new(),
            ConversationType::ServerDm,
            Some(sender),
            "hi".into(),
            1,
        )
    };

    // Ours: silent whatever else holds.
    assert!(!should_notify_message(&msg(me), Some(&me), false, false));
    // Identity unknown (e.g. AuthState never set): fail closed.
    assert!(!should_notify_message(&msg(peer), None, false, false));
    // A peer's message in a background chat notifies…
    let m = msg(peer);
    assert!(should_notify_message(&m, Some(&me), false, false));
    // …but not in the chat on screen, nor in a muted one.
    assert!(!should_notify_message(&m, Some(&me), true, false));
    assert!(!should_notify_message(&m, Some(&me), false, true));
    // A call record filed under the peer is not a message to read.
    let mut call = msg(peer);
    call.content = crate::domain::message::MessageContent::Call(
        crate::domain::message::CallMessage::started("c".into(), peer, "Peer".into()),
    );
    assert!(!should_notify_message(&call, Some(&me), false, false));
}

#[test]
fn test_default_detail_level_is_none() {
    let service = NotificationService::noop();
    assert_eq!(service.detail_level(), NotificationDetailLevel::None);
}

#[test]
fn test_set_detail_level() {
    let service = NotificationService::noop();
    service.set_detail_level(NotificationDetailLevel::NameAndMessage);
    assert_eq!(
        service.detail_level(),
        NotificationDetailLevel::NameAndMessage
    );
}

#[test]
fn test_detail_level_from_str() {
    assert_eq!(
        NotificationDetailLevel::from_str("none"),
        NotificationDetailLevel::None
    );
    assert_eq!(
        NotificationDetailLevel::from_str("name_only"),
        NotificationDetailLevel::NameOnly
    );
    assert_eq!(
        NotificationDetailLevel::from_str("name_and_message"),
        NotificationDetailLevel::NameAndMessage
    );
    assert_eq!(
        NotificationDetailLevel::from_str("garbage"),
        NotificationDetailLevel::None
    );
    assert_eq!(
        NotificationDetailLevel::from_str(""),
        NotificationDetailLevel::None
    );
}

#[test]
fn test_detail_level_as_str() {
    assert_eq!(NotificationDetailLevel::None.as_str(), "none");
    assert_eq!(NotificationDetailLevel::NameOnly.as_str(), "name_only");
    assert_eq!(
        NotificationDetailLevel::NameAndMessage.as_str(),
        "name_and_message"
    );
}

/// The level decides what reaches the platform sink (desktop notify-rust
/// and Android JNI both render title/body verbatim), and a sealed-sender
/// message never names a sender whatever the level.
#[test]
fn detail_level_shapes_what_reaches_the_sink() {
    struct Capture(std::sync::Mutex<Vec<(String, String)>>);
    impl NotificationSink for Capture {
        fn show(
            &self,
            title: &str,
            body: &str,
            _: NotificationPriority,
        ) -> Result<(), SystemError> {
            self.0.lock().unwrap().push((title.into(), body.into()));
            Ok(())
        }
    }
    let sink = Arc::new(Capture(Default::default()));
    let service = NotificationService::new(sink.clone());
    let conversation = crate::domain::ids::ChannelId::new();
    let shown = |level, sealed| {
        service.set_detail_level(level);
        service
            .show_message_notification(
                &conversation,
                Some("Alice"),
                Some("are you free?"),
                sealed,
                NotificationPriority::Normal,
            )
            .unwrap();
        sink.0.lock().unwrap().pop().unwrap()
    };
    let pair = |t: &str, b: &str| (t.to_string(), b.to_string());

    assert_eq!(
        shown(NotificationDetailLevel::NameAndMessage, false),
        pair("Alice", "are you free?")
    );
    assert_eq!(
        shown(NotificationDetailLevel::NameOnly, false),
        pair("Alice", "New message")
    );
    assert_eq!(
        shown(NotificationDetailLevel::None, false),
        pair("Chatter", "New message")
    );
    // Private (sealed / secret chat): generic at every level.
    for level in [
        NotificationDetailLevel::None,
        NotificationDetailLevel::NameOnly,
        NotificationDetailLevel::NameAndMessage,
    ] {
        assert_eq!(shown(level, true), pair("Chatter", "New message"));
    }
}

/// Secret chats store decoded Text/Mixed with a sender, so the
/// conversation type — not sealed content — must mark them private.
/// The reaction path classifies its target message the same way.
#[test]
fn secret_chat_messages_and_reaction_targets_are_private() {
    use crate::domain::ids::{ChannelId, UserId};
    use crate::domain::message::{ConversationType, Message, MessageContent, MixedContent};

    let msg = |ct| Message::new_text(ChannelId::new(), ct, Some(UserId::new()), "hi".into(), 1);
    for ct in [ConversationType::AnonDm, ConversationType::AnonGroup] {
        let text = msg(ct.clone());
        assert!(is_private_notification(&text), "{ct:?} text");
        let mut mixed = msg(ct.clone());
        mixed.content = MessageContent::Mixed(MixedContent::text_only("hi".into()));
        assert!(is_private_notification(&mixed), "{ct:?} mixed");
    }
    assert!(!is_private_notification(&msg(ConversationType::ServerDm)));
    assert!(!is_private_notification(&msg(ConversationType::Channel)));
}

#[tokio::test]
async fn secret_request_and_message_events_reach_generic_notification_sink() {
    use crate::domain::anon_dm::AnonDmId;
    use crate::domain::ids::{ChannelId, UserId};
    use crate::domain::message::{ConversationType, Message};
    use crate::inbox::handlers::DomainEvent;

    struct Capture(tokio::sync::mpsc::UnboundedSender<(String, String)>);
    impl NotificationSink for Capture {
        fn show(
            &self,
            title: &str,
            body: &str,
            _: NotificationPriority,
        ) -> Result<(), SystemError> {
            self.0.send((title.into(), body.into())).unwrap();
            Ok(())
        }
    }
    static KEYRING: std::sync::Once = std::sync::Once::new();
    KEYRING.call_once(|| {
        keyring::set_default_store(crate::system::keyring::test_credential_store());
    });
    let dir = tempfile::tempdir().unwrap();
    let client = crate::client::ChatterClient::builder()
        .server_url("https://localhost:1")
        .data_dir(dir.path().to_str().unwrap())
        .build()
        .unwrap();
    let me = UserId::new();
    client
        .messages()
        .settings_repo
        .set_string(
            crate::storage::repositories::settings_repo::keys::CURRENT_USER_ID,
            &me.to_string(),
        )
        .unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let service = Arc::new(NotificationService::new(Arc::new(Capture(tx))));
    let bus = crate::events::EventBus::new();
    let task = spawn_notification_listener(
        bus.clone(),
        Arc::downgrade(&client),
        client.users().clone(),
        service.clone(),
    );
    let id = AnonDmId::new();
    let peer = UserId::new();
    client
        .chat_state()
        .set_active_conversation(crate::state::ActiveConversation::AnonDm(id));
    // An open conversation in a backgrounded app must still notify.
    client.chat_state().set_app_visible(false);
    for level in [
        NotificationDetailLevel::None,
        NotificationDetailLevel::NameOnly,
        NotificationDetailLevel::NameAndMessage,
    ] {
        service.set_detail_level(level);
        for event in [
            DomainEvent::AnonDmRequestReceived {
                conversation_id: id,
                sender_id: peer,
                sender_mailbox_pk: [1; 32],
                intro_message: Some("private introduction".into()),
                profile_key: None,
                sender_display_name: Some("private name".into()),
                sender_avatar: None,
            },
            DomainEvent::MessageReceived {
                message: Message::new_text(
                    ChannelId::from(id),
                    ConversationType::AnonDm,
                    Some(peer),
                    "private message".into(),
                    1,
                ),
            },
            DomainEvent::MessageReceived {
                message: Message::new_text(
                    ChannelId::new(),
                    ConversationType::AnonGroup,
                    Some(peer),
                    "private group message".into(),
                    1,
                ),
            },
        ] {
            bus.emit_domain(event);
            let shown = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(shown, ("Chatter".into(), "New message".into()));
        }
    }
    for suppression in ["foreground", "muted", "self"] {
        client
            .chat_state()
            .set_app_visible(suppression == "foreground");
        client
            .mute_conversation(
                crate::state::ActiveConversation::AnonDm(id),
                suppression == "muted",
            )
            .unwrap();
        bus.emit_domain(DomainEvent::AnonDmRequestReceived {
            conversation_id: id,
            sender_id: if suppression == "self" { me } else { peer },
            sender_mailbox_pk: [1; 32],
            intro_message: Some("private introduction".into()),
            profile_key: None,
            sender_display_name: Some("private name".into()),
            sender_avatar: None,
        });
        // A following event is a barrier: a request notification would
        // reach the sink before this device notification.
        bus.emit_domain(DomainEvent::DeviceLinked {
            device_id: 2,
            device_name: None,
            device_info: None,
            approved_by_self: false,
        });
        let shown = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(shown.0, "New device linked", "{suppression}");
        assert!(rx.try_recv().is_err());
    }
    task.abort();
    task.await.unwrap_err();
}

#[tokio::test]
async fn secret_request_ingestion_notifies_once_and_keeps_catch_up_requests() {
    use crate::domain::anon_dm::AnonDmId;
    use crate::domain::ids::UserId;
    use crate::inbox::events::InboxEvent;
    use crate::inbox::handlers::DomainEvent;

    struct Capture(tokio::sync::mpsc::UnboundedSender<(String, String)>);
    impl NotificationSink for Capture {
        fn show(
            &self,
            title: &str,
            body: &str,
            _: NotificationPriority,
        ) -> Result<(), SystemError> {
            self.0.send((title.into(), body.into())).unwrap();
            Ok(())
        }
    }
    keyring::set_default_store(crate::system::keyring::test_credential_store());
    let dir = tempfile::tempdir().unwrap();
    let client = crate::client::ChatterClient::builder()
        .server_url("https://localhost:1")
        .data_dir(dir.path().to_str().unwrap())
        .build()
        .unwrap();
    client
        .messages()
        .settings_repo
        .set_string(
            crate::storage::repositories::settings_repo::keys::CURRENT_USER_ID,
            &UserId::new().to_string(),
        )
        .unwrap();
    client.chat_state().set_app_visible(false);
    let bus = client.event_bus().clone();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let listener = spawn_notification_listener(
        bus.clone(),
        Arc::downgrade(&client),
        client.users().clone(),
        Arc::new(NotificationService::new(Arc::new(Capture(tx)))),
    );
    let handler = crate::events::EventHandlers::new(
        client.message_state().clone(),
        client.chat_state().clone(),
    )
    .with_anon_dm_service(client.anon_dms().clone())
    .spawn(bus.clone());
    let processor = crate::services::inbox_processor::InboxEventProcessor::from_client(&client);
    let peer = UserId::new();
    let mut arrivals = bus.subscribe_domain();
    let sealed = InboxEvent::AnonDmRequest {
        peer_id: peer,
        sender_device_id: 2,
        conversation_id: AnonDmId::new().to_vec(),
        initiator_mailbox_pk: vec![1; 32],
        intro_message: Some("private introduction".into()),
        profile_key: None,
        encrypted_display_name: None,
        avatar: None,
        timestamp: 1,
    };
    processor.process(&sealed).await;
    let shown = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(shown, ("Chatter".into(), "New message".into()));
    // The private friend-code path enters through a decrypted init domain
    // event, rather than the sealed-request InboxEvent handled above.
    let init = DomainEvent::AnonDmInitReceived {
        conversation_id: AnonDmId::new(),
        sender_id: peer,
        sender_device_id: 2,
        sender_display_name: "private name".into(),
        sender_avatar: None,
        peer_mailbox_pk: [2; 32],
        peer_identity_key: [5; 33],
        peer_delivery_token: [3; 12],
        intro_message: "private introduction".into(),
        server_timestamp: 1,
    };
    bus.emit_domain(init.clone());
    let shown = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(shown, ("Chatter".into(), "New message".into()));
    // Catch-up can replay an existing request immediately before a new
    // request from the same peer. Only the conversation id deduplicates.
    bus.emit_domain(init.clone());
    let mut next = init;
    let next_id = AnonDmId::new();
    if let DomainEvent::AnonDmInitReceived {
        conversation_id, ..
    } = &mut next
    {
        *conversation_id = next_id;
    }
    bus.emit_domain(next);
    let shown = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(shown, ("Chatter".into(), "New message".into()));
    // Wait for the final derived arrival to prove the handler drained both
    // init events, then place a barrier on the notification listener.
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if matches!(arrivals.recv().await.unwrap(), DomainEvent::AnonDmRequestReceived { conversation_id, .. } if conversation_id == next_id) { break; }
            }
        }).await.unwrap();
    bus.emit_domain(DomainEvent::DeviceLinked {
        device_id: 3,
        device_name: None,
        device_info: None,
        approved_by_self: false,
    });
    let shown = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        shown.0, "New device linked",
        "duplicate init must not notify"
    );
    assert!(rx.try_recv().is_err());
    handler.abort();
    listener.abort();
}

/// A panicking sink is one failed notification, not a dead caller loop.
#[test]
fn a_panicking_sink_is_an_error_not_a_panic() {
    struct Panics;
    impl NotificationSink for Panics {
        fn show(&self, _: &str, _: &str, _: NotificationPriority) -> Result<(), SystemError> {
            Ok(())
        }
        fn show_incoming_call(&self, _: &str, _: &str, _: &str) -> Result<(), SystemError> {
            panic!("attempt to multiply with overflow")
        }
    }
    let service = NotificationService::new(Arc::new(Panics));
    assert!(service.show_incoming_call("c", "t", "b").is_err());
    assert!(service
        .show_message("t", "b", NotificationPriority::Normal)
        .is_ok());
}

#[test]
fn notification_key_is_stable_and_hides_the_id() {
    let id = crate::domain::ids::ChannelId::new();
    assert_eq!(notification_key(&id), notification_key(&id));
    assert_ne!(
        notification_key(&id),
        notification_key(&crate::domain::ids::ChannelId::new())
    );
    assert!(!notification_key(&id).contains(&id.to_string()));
    assert!(is_opaque_notification_key(&notification_key(&id)));
    assert!(!is_opaque_notification_key(&id.to_string()));
    assert!(!is_opaque_notification_key("Alice: secret text"));
}

#[test]
fn notification_key_resolves_only_visible_conversations() {
    use crate::domain::message::ConversationType;
    use crate::state::ActiveConversation;

    let channel = crate::domain::ids::ChannelId::new();
    let secret = crate::domain::anon_dm::AnonDmId::new();
    let rows = vec![
        (channel.to_string(), ConversationType::Channel),
        (secret.to_string(), ConversationType::AnonDm),
    ];
    assert_eq!(
        conversation_for_notification_key(&notification_key(&channel), rows.clone()),
        Some(ActiveConversation::Channel(channel))
    );
    assert_eq!(
        conversation_for_notification_key(&notification_key(&secret.into()), rows.clone()),
        Some(ActiveConversation::AnonDm(secret))
    );
    assert_eq!(
        conversation_for_notification_key(&notification_key(&channel), rows[1..].to_vec()),
        None,
        "a Shield-hidden channel cannot be opened from a notification"
    );
    assert_eq!(
        conversation_for_notification_key(&channel.to_string(), rows),
        None
    );
}

#[test]
fn private_notification_passes_only_opaque_key_and_generic_text_to_platform() {
    struct Capture(std::sync::Mutex<Option<(String, String, String)>>);
    impl NotificationSink for Capture {
        fn show(&self, _: &str, _: &str, _: NotificationPriority) -> Result<(), SystemError> {
            unreachable!()
        }
        fn show_conversation(
            &self,
            key: &str,
            title: &str,
            body: &str,
            _: NotificationPriority,
        ) -> Result<(), SystemError> {
            *self.0.lock().unwrap() = Some((key.into(), title.into(), body.into()));
            Ok(())
        }
    }
    let sink = Arc::new(Capture(Default::default()));
    let service = NotificationService::new(sink.clone());
    service.set_detail_level(NotificationDetailLevel::NameAndMessage);
    let id = crate::domain::ids::ChannelId::new();
    service
        .show_message_notification(
            &id,
            Some("Alice"),
            Some("secret text"),
            true,
            NotificationPriority::Normal,
        )
        .unwrap();
    let (key, title, body) = sink.0.lock().unwrap().take().unwrap();
    assert_eq!(key, notification_key(&id));
    assert_eq!((title.as_str(), body.as_str()), ("Chatter", "New message"));
    assert!(!key.contains(&id.to_string()));
    assert!(!key.contains("Alice"));
    assert!(!key.contains("secret text"));
}

#[test]
fn test_detail_level_roundtrip() {
    for level in [
        NotificationDetailLevel::None,
        NotificationDetailLevel::NameOnly,
        NotificationDetailLevel::NameAndMessage,
    ] {
        assert_eq!(NotificationDetailLevel::from_str(level.as_str()), level);
    }
}

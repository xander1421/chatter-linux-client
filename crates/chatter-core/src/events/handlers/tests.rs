use super::*;
use crate::domain::ids::{ChannelId, UserId};
use crate::domain::message::{ConversationType, Message, MessageContent};

fn create_handlers() -> EventHandlers {
    EventHandlers::new(MessageState::new(), ChatState::new())
}

fn create_test_message(channel_id: ChannelId) -> Message {
    Message::new_text(
        channel_id,
        ConversationType::Channel,
        Some(UserId::new()),
        "Test message".to_string(),
        0,
    )
}

#[tokio::test]
async fn real_bootstrap_events_and_anonymous_success_preserve_persisted_receipts() {
    let dir = tempfile::tempdir().unwrap();
    // Opt out before client construction so this fixture never uses a keyring.
    let db_path = crate::system::Directories::with_base_dir(dir.path().to_path_buf())
        .unwrap()
        .database_path();
    std::fs::write(
        db_path
            .parent()
            .unwrap()
            .join(crate::system::keyring::NO_KEYRING_MARKER),
        b"",
    )
    .unwrap();
    let client = crate::client::ChatterClient::builder()
        .server_url("https://localhost:1")
        .data_dir(dir.path().to_str().unwrap())
        .build()
        .unwrap();
    let mut handlers = create_handlers();
    handlers.message_service = Some(client.messages().clone());
    let peer = UserId::new();
    for receipt in ["sent", "delivered", "read"] {
        let mut row = Message::new_text(
            peer.into(),
            ConversationType::ServerDm,
            Some(UserId::new()),
            "original".into(),
            51,
        );
        row.delivery_status = receipt.into();
        client.messages().store_message_local(&row).unwrap();
        handlers.handle_domain_event(DomainEvent::DmBootstrapCompleted {
            peer_id: peer,
            sent_timestamps: vec![51],
        });
        handlers.handle_domain_event(DomainEvent::MessageSendFailed {
            channel_id: peer.into(),
            error: "late failure".into(),
            message_timestamps: vec![51],
        });
        assert_eq!(
            client
                .messages()
                .get_message_local(&row.id)
                .unwrap()
                .unwrap()
                .delivery_status,
            receipt
        );
        client.messages().delete_message_local(&row.id).unwrap();
    }
    // This is the actual repository entry used by anonymous-DM success.
    let repo = crate::storage::repositories::UnifiedMessageRepository::new(
        client.messages().database.clone(),
    );
    let channel = ChannelId::new();
    let mut row = Message::new_text(
        channel,
        ConversationType::AnonDm,
        Some(UserId::new()),
        "anonymous".into(),
        52,
    );
    row.delivery_status = "read".into();
    repo.insert_outgoing(&row).unwrap();
    for status in ["sent", "failed", "delivered"] {
        repo.update_delivery_status_by_timestamps(
            &channel,
            &ConversationType::AnonDm,
            &[52],
            status,
        )
        .unwrap();
        assert_eq!(repo.get(&row.id).unwrap().unwrap().delivery_status, "read");
    }
}

// ========================================================================
// Message Event Tests
// ========================================================================

#[test]
fn test_handle_message_received() {
    let handlers = create_handlers();
    let channel_id = ChannelId::new();
    let message = create_test_message(channel_id);
    let message_id = message.id;

    handlers.handle_domain_event(DomainEvent::MessageReceived {
        message: message.clone(),
    });

    let messages = handlers.message_state.get_messages(&channel_id);
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].id, message_id);
}

#[test]
fn test_handle_message_edited() {
    let handlers = create_handlers();
    let channel_id = ChannelId::new();
    let message = create_test_message(channel_id);
    let message_id = message.id;

    // First add the message
    handlers.handle_domain_event(DomainEvent::MessageReceived { message });

    // Then edit it
    let new_content = MessageContent::Text("Edited content".to_string());
    handlers.handle_domain_event(DomainEvent::MessageEdited {
        message_id,
        new_content: new_content.clone(),
        channel_id,
        conversation_type: ConversationType::Channel,
        author_id: UserId::new(),
        target_sent_timestamp: 0,
    });

    let messages = handlers.message_state.get_messages(&channel_id);
    assert_eq!(messages.len(), 1);
    assert!(messages[0].is_edited);
    assert_eq!(messages[0].content, new_content);
}

/// A Saved edit event carries no authority: without the stored row to
/// read back, nothing is shown or written.
#[test]
fn saved_edit_event_never_applies_its_own_body() {
    let handlers = create_handlers();
    let channel_id = ChannelId::new();
    let mut message = create_test_message(channel_id);
    message.conversation_type = ConversationType::SavedMessages;
    let message_id = message.id;
    let original = message.content.clone();
    handlers.handle_domain_event(DomainEvent::MessageReceived { message });
    handlers.handle_domain_event(DomainEvent::MessageEdited {
        message_id,
        new_content: MessageContent::Text("queued older body".to_string()),
        channel_id,
        conversation_type: ConversationType::SavedMessages,
        author_id: UserId::new(),
        target_sent_timestamp: 0,
    });
    let messages = handlers.message_state.get_messages(&channel_id);
    assert_eq!(messages[0].content, original);
    assert!(!messages[0].is_edited);
}

#[test]
fn test_handle_message_deleted() {
    let handlers = create_handlers();
    let channel_id = ChannelId::new();
    let message = create_test_message(channel_id);
    let message_id = message.id;
    let author_id = UserId::new();

    // First add the message
    handlers.handle_domain_event(DomainEvent::MessageReceived { message });
    assert_eq!(handlers.message_state.get_messages(&channel_id).len(), 1);

    // Then delete it
    handlers.handle_domain_event(DomainEvent::MessageDeleted {
        message_id,
        author_id,
        channel_id: Some(channel_id),
        target_sent_timestamp: 1704067200_000_000_000,
    });

    assert!(handlers.message_state.get_messages(&channel_id).is_empty());
}

// ========================================================================
// Typing Event Tests
// ========================================================================

#[test]
fn test_handle_typing_started() {
    let handlers = create_handlers();
    let channel_id = ChannelId::new();
    let user_id = UserId::new();

    handlers.handle_domain_event(DomainEvent::TypingStarted {
        user_id,
        channel_id,
        timestamp: 0,
    });

    let typers = handlers.chat_state.get_typing_users(&channel_id);
    assert_eq!(typers.len(), 1);
    assert!(typers.contains(&user_id));
}

#[test]
fn test_handle_typing_stopped() {
    let handlers = create_handlers();
    let channel_id = ChannelId::new();
    let user_id = UserId::new();

    // Start typing
    handlers.handle_domain_event(DomainEvent::TypingStarted {
        user_id,
        channel_id,
        timestamp: 0,
    });
    assert_eq!(handlers.chat_state.get_typing_users(&channel_id).len(), 1);

    // Stop typing
    handlers.handle_domain_event(DomainEvent::TypingStopped {
        user_id,
        channel_id,
        timestamp: 0,
    });
    assert!(handlers.chat_state.get_typing_users(&channel_id).is_empty());
}

#[test]
fn test_handle_typing_multiple_users() {
    let handlers = create_handlers();
    let channel_id = ChannelId::new();
    let user1 = UserId::new();
    let user2 = UserId::new();

    handlers.handle_domain_event(DomainEvent::TypingStarted {
        user_id: user1,
        channel_id,
        timestamp: 0,
    });
    handlers.handle_domain_event(DomainEvent::TypingStarted {
        user_id: user2,
        channel_id,
        timestamp: 0,
    });

    let typers = handlers.chat_state.get_typing_users(&channel_id);
    assert_eq!(typers.len(), 2);
    assert!(typers.contains(&user1));
    assert!(typers.contains(&user2));

    // Remove one
    handlers.handle_domain_event(DomainEvent::TypingStopped {
        user_id: user1,
        channel_id,
        timestamp: 0,
    });

    let typers = handlers.chat_state.get_typing_users(&channel_id);
    assert_eq!(typers.len(), 1);
    assert!(!typers.contains(&user1));
    assert!(typers.contains(&user2));
}

#[test]
fn test_handle_typing_duplicate_start() {
    let handlers = create_handlers();
    let channel_id = ChannelId::new();
    let user_id = UserId::new();

    // Start typing twice (should not duplicate)
    handlers.handle_domain_event(DomainEvent::TypingStarted {
        user_id,
        channel_id,
        timestamp: 0,
    });
    handlers.handle_domain_event(DomainEvent::TypingStarted {
        user_id,
        channel_id,
        timestamp: 0,
    });

    let typers = handlers.chat_state.get_typing_users(&channel_id);
    assert_eq!(typers.len(), 1);
}

// ========================================================================
// Reaction Event Tests
// ========================================================================

#[test]
fn test_handle_reaction_added() {
    let handlers = create_handlers();
    let sender_id = UserId::new();
    let sent_at_ns: u64 = 1704067200_000_000_000;
    let reactor_id = UserId::new();

    handlers.handle_domain_event(DomainEvent::ReactionAdded {
        scope: crate::domain::reaction::ReactionScope::new(
            crate::domain::ChannelId::from(sender_id),
            crate::domain::message::ConversationType::ServerDm,
        ),
        sender_id,
        sent_at_ns,
        reactor_id,
        emoji: "👍".to_string(),
        timestamp: 1234567890,
    });

    let cache_key = crate::domain::reaction::ReactionScope::new(
        crate::domain::ChannelId::from(sender_id),
        crate::domain::message::ConversationType::ServerDm,
    )
    .cache_key(&sender_id, sent_at_ns);
    let reactions = handlers.chat_state.get_reactions(&cache_key);
    assert!(
        reactions.is_none(),
        "an event without a persisted repository is not count authority"
    );
}

#[test]
fn test_handle_reaction_removed() {
    let handlers = create_handlers();
    let sender_id = UserId::new();
    let sent_at_ns: u64 = 1704067200_000_000_000;
    let reactor_id = UserId::new();

    // Add reaction
    handlers.handle_domain_event(DomainEvent::ReactionAdded {
        scope: crate::domain::reaction::ReactionScope::new(
            crate::domain::ChannelId::from(sender_id),
            crate::domain::message::ConversationType::ServerDm,
        ),
        sender_id,
        sent_at_ns,
        reactor_id,
        emoji: "👍".to_string(),
        timestamp: 1234567890,
    });

    // Remove reaction
    handlers.handle_domain_event(DomainEvent::ReactionRemoved {
        scope: crate::domain::reaction::ReactionScope::new(
            crate::domain::ChannelId::from(sender_id),
            crate::domain::message::ConversationType::ServerDm,
        ),
        sender_id,
        sent_at_ns,
        reactor_id,
        emoji: "👍".to_string(),
        timestamp: 1234567891,
    });

    // Should be removed entirely when count hits 0
    let cache_key = crate::domain::reaction::ReactionScope::new(
        crate::domain::ChannelId::from(sender_id),
        crate::domain::message::ConversationType::ServerDm,
    )
    .cache_key(&sender_id, sent_at_ns);
    let reactions = handlers.chat_state.get_reactions(&cache_key);
    assert!(reactions.is_none());
}

// ========================================================================
// Pin Event Tests
// ========================================================================

#[test]
fn test_handle_message_pinned() {
    let handlers = create_handlers();
    let channel_id = ChannelId::new();
    let message = create_test_message(channel_id);
    let message_id = message.id;
    let pinner_id = UserId::new();

    // Add message first
    handlers.handle_domain_event(DomainEvent::MessageReceived { message });

    // Pin it
    handlers.handle_domain_event(DomainEvent::MessagePinned {
        message_id,
        pinner_id,
        channel_id,
        conversation_type: ConversationType::Channel,
        target_sent_timestamp: 0,
    });

    // Check message state
    let messages = handlers.message_state.get_messages(&channel_id);
    assert!(messages[0].is_pinned);

    // Check pinned state
    assert_eq!(handlers.chat_state.pinned_total_count(), 1);
    assert_eq!(
        handlers.chat_state.pinned_current_message_id(),
        Some(message_id)
    );
}

#[test]
fn test_handle_message_unpinned() {
    let handlers = create_handlers();
    let channel_id = ChannelId::new();
    let message = create_test_message(channel_id);
    let message_id = message.id;
    let pinner_id = UserId::new();

    // Add and pin message
    handlers.handle_domain_event(DomainEvent::MessageReceived { message });
    handlers.handle_domain_event(DomainEvent::MessagePinned {
        message_id,
        pinner_id,
        channel_id,
        conversation_type: ConversationType::Channel,
        target_sent_timestamp: 0,
    });

    // Unpin it
    handlers.handle_domain_event(DomainEvent::MessageUnpinned {
        message_id,
        unpinner_id: pinner_id,
        channel_id,
        conversation_type: ConversationType::Channel,
        target_sent_timestamp: 0,
    });

    // Check message state
    let messages = handlers.message_state.get_messages(&channel_id);
    assert!(!messages[0].is_pinned);

    // Check pinned state
    assert_eq!(handlers.chat_state.pinned_total_count(), 0);
}

// ========================================================================
// Sync Event Tests
// ========================================================================

#[test]
fn test_handle_delete_for_me_sync() {
    let handlers = create_handlers();
    let channel_id = ChannelId::new();
    let message = create_test_message(channel_id);
    let message_id = message.id;
    let author_id = UserId::new();

    // Add message
    handlers.handle_domain_event(DomainEvent::MessageReceived { message });
    assert_eq!(handlers.message_state.get_messages(&channel_id).len(), 1);

    // Sync delete from another device
    handlers.handle_domain_event(DomainEvent::DeleteForMeSync {
        conversation_id: channel_id,
        target_author_id: author_id,
        target_message_id: message_id,
        timestamp: 1234567890,
        saved_deletion_persisted: false,
    });

    // Message should be removed
    assert!(handlers.message_state.get_messages(&channel_id).is_empty());
}

// ========================================================================
// Spawn Tests
// ========================================================================

#[tokio::test]
async fn test_spawn_receives_events() {
    let handlers = create_handlers();
    let bus = EventBus::new();

    let _handle = handlers.spawn(bus.clone());

    let channel_id = ChannelId::new();
    let message = create_test_message(channel_id);

    // Give the spawned task time to subscribe
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;

    // Emit an event
    bus.emit_domain(DomainEvent::MessageReceived {
        message: message.clone(),
    });

    // Give time for event to be processed
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // Check that the message was added to state
    let messages = handlers.message_state.get_messages(&channel_id);
    assert_eq!(messages.len(), 1);
}

#[tokio::test]
async fn test_spawn_handles_bus_close() {
    let handlers = create_handlers();
    let bus = EventBus::new();

    let handle = handlers.spawn(bus.clone());

    // Drop the bus
    drop(bus);

    // The handler should complete without panic
    let result = tokio::time::timeout(std::time::Duration::from_secs(1), handle).await;
    assert!(result.is_ok());
}

// ========================================================================
// Debug Impl Test
// ========================================================================

#[test]
fn test_debug_impl() {
    let handlers = create_handlers();
    let debug_str = format!("{:?}", handlers);
    assert!(debug_str.contains("EventHandlers"));
    assert!(debug_str.contains("message_state"));
}
#[tokio::test]
async fn reaction_events_refresh_persisted_scoped_cache_and_duplicate_snapshot_never_adds() {
    use crate::domain::message::{ConversationType, Message};
    use crate::domain::reaction::ReactionScope;
    use crate::domain::{ChannelId, UserId};
    static MOCK: std::sync::Once = std::sync::Once::new();
    MOCK.call_once(|| keyring::set_default_store(crate::system::keyring::test_credential_store()));
    let dir = tempfile::tempdir().unwrap();
    let client = crate::client::ChatterClient::builder()
        .server_url("https://localhost:1")
        .data_dir(dir.path().to_str().unwrap())
        .build()
        .unwrap();
    let own = UserId::new();
    let author = UserId::new();
    client
        .messages()
        .settings_repo
        .set("auth.current_user_id", &own.to_string())
        .unwrap();
    let id = ChannelId::new();
    let scope = ReactionScope::new(id, ConversationType::ServerDm);
    let foreign = ReactionScope::new(id, ConversationType::AnonGroup);
    for scope in [&scope, &foreign] {
        client
            .messages()
            .message_repo
            .insert(&Message::new_text(
                scope.id,
                scope.kind.clone(),
                Some(author),
                "same author clock".into(),
                100,
            ))
            .unwrap();
    }
    let chat = ChatState::new();
    let handlers = EventHandlers::new(MessageState::new(), chat.clone())
        .with_message_service(client.messages().clone());
    let key = scope.cache_key(&author, 100);
    let other_key = foreign.cache_key(&author, 100);
    chat.update_reaction(&key, "👍".into(), 99, None);
    chat.update_reaction(&other_key, "👍".into(), 77, None);
    client
        .messages()
        .record_reaction(&scope, &author, 100, &own, "👍", false)
        .unwrap();
    let event = DomainEvent::ReactionSnapshotChanged {
        scope: scope.clone(),
        sender_id: author,
        sent_at_ns: 100,
    };
    handlers.handle_domain_event(event.clone());
    handlers.handle_domain_event(event.clone());
    let shown = chat.get_reactions(&key).unwrap();
    assert_eq!(shown[0].count, 1);
    assert!(shown[0].user_reacted);
    assert_eq!(
        chat.get_reactions(&other_key).unwrap()[0].count,
        77,
        "kind collision stays separate"
    );
    client
        .messages()
        .record_reaction(&scope, &author, 100, &own, "👍", true)
        .unwrap();
    handlers.handle_domain_event(event);
    assert!(chat.get_reactions(&key).is_none());
    assert_eq!(chat.get_reactions(&other_key).unwrap()[0].count, 77);
}

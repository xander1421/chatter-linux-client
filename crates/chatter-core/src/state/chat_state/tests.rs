#[cfg(test)]
mod tests {
    use super::super::*;

    #[test]
    fn is_for_channel_matches_only_its_own_conversation() {
        let (peer, other) = (UserId::new(), UserId::new());
        let server_dm = ActiveConversation::ServerDm { peer_id: peer };
        assert!(server_dm.is_for_channel(ChannelId::from(peer)));
        assert!(!server_dm.is_for_channel(ChannelId::from(other)));

        let channel = ChannelId::new();
        assert!(ActiveConversation::Channel(channel).is_for_channel(channel));
        assert!(!ActiveConversation::Channel(channel).is_for_channel(ChannelId::new()));
        assert!(!ActiveConversation::None.is_for_channel(channel));
    }

    /// Call history stores `routing_key()` and redials through
    /// `from_routing_key()`. Every conversation that can host a call has to
    /// survive that round trip, or "call back" opens the wrong thing.
    #[test]
    fn routing_key_round_trips_every_call_capable_conversation() {
        let cases = [
            ActiveConversation::Channel(ChannelId::new()),
            ActiveConversation::ServerDm {
                peer_id: UserId::new(),
            },
            ActiveConversation::AnonDm(AnonDmId::new()),
            ActiveConversation::AnonGroup(AnonGroupId::new()),
        ];
        for conv in cases {
            let (ct, id) = conv
                .routing_key()
                .expect("call-capable conversation has a key");
            assert_eq!(ActiveConversation::from_routing_key(&ct, &id), Some(conv));
        }
        assert!(ActiveConversation::None.routing_key().is_none());
    }

    /// A fresh launch is in view; the platform lifecycle flips the flag.
    #[test]
    fn app_visible_starts_true_and_follows_lifecycle() {
        let state = ChatState::new();
        assert!(state.is_app_visible());
        assert!(state.set_app_visible(false), "starts visible");
        assert!(!state.is_app_visible());
        assert!(!state.set_app_visible(true));
        assert!(state.is_app_visible());
    }

    #[test]
    fn test_chat_state_default() {
        let state = ChatState::new();
        let data = state.get();

        assert!(data.active_channel_id.is_none());
        assert!(data.open_threads.is_empty());
        assert!(data.active_thread_index.is_none());
    }

    #[test]
    fn test_set_active_channel() {
        let state = ChatState::new();
        let channel_id = ChannelId::new();

        state.set_active_channel(channel_id);
        let data = state.get();

        assert_eq!(data.active_channel_id, Some(channel_id));
    }

    #[test]
    fn test_open_thread() {
        let state = ChatState::new();
        let channel_id = ChannelId::new();
        let root_id = MessageId::new();

        state.open_thread(channel_id, root_id);
        let data = state.get();

        assert_eq!(data.open_threads.len(), 1);
        assert_eq!(data.open_threads[0].root_message_id, root_id);
        assert_eq!(data.active_thread_index, Some(0));
    }

    #[test]
    fn test_open_thread_max_limit() {
        let state = ChatState::new();
        let channel_id = ChannelId::new();

        let id1 = MessageId::new();
        let id2 = MessageId::new();
        let id3 = MessageId::new();
        let id4 = MessageId::new();

        state.open_thread(channel_id, id1);
        state.open_thread(channel_id, id2);
        state.open_thread(channel_id, id3);
        assert_eq!(state.get().open_threads.len(), 3);

        // Opening 4th should replace the oldest (id1)
        state.open_thread(channel_id, id4);
        let data = state.get();

        assert_eq!(data.open_threads.len(), 3);
        assert!(!data.open_threads.iter().any(|t| t.root_message_id == id1));
        assert!(data.open_threads.iter().any(|t| t.root_message_id == id4));
    }

    #[test]
    fn test_open_existing_thread_focuses_it() {
        let state = ChatState::new();
        let channel_id = ChannelId::new();

        let id1 = MessageId::new();
        let id2 = MessageId::new();

        state.open_thread(channel_id, id1);
        state.open_thread(channel_id, id2);
        assert_eq!(state.get().active_thread_index, Some(1));

        // Re-opening id1 should focus it without adding a new one
        state.open_thread(channel_id, id1);
        let data = state.get();

        assert_eq!(data.open_threads.len(), 2);
        assert_eq!(data.active_thread_index, Some(0));
    }

    #[test]
    fn test_close_thread() {
        let state = ChatState::new();
        let channel_id = ChannelId::new();

        let id1 = MessageId::new();
        let id2 = MessageId::new();

        state.open_thread(channel_id, id1);
        state.open_thread(channel_id, id2);
        state.close_thread(id1);

        let data = state.get();
        assert_eq!(data.open_threads.len(), 1);
        assert_eq!(data.open_threads[0].root_message_id, id2);
    }

    #[test]
    fn test_active_thread() {
        let state = ChatState::new();
        let channel_id = ChannelId::new();
        let root_id = MessageId::new();

        assert!(state.active_thread().is_none());

        state.open_thread(channel_id, root_id);
        let active = state.active_thread();

        assert!(active.is_some());
        assert_eq!(active.unwrap().root_message_id, root_id);
    }

    #[test]
    fn test_update_thread_reply_count() {
        let state = ChatState::new();
        let channel_id = ChannelId::new();
        let root_id = MessageId::new();

        state.open_thread(channel_id, root_id);
        state.update_thread_reply_count(root_id, 42);

        let data = state.get();
        assert_eq!(data.open_threads[0].reply_count, 42);
    }

    #[tokio::test]
    async fn test_chat_state_subscribe() {
        let state = ChatState::new();
        let mut rx = state.subscribe();
        let channel_id = ChannelId::new();

        state.set_active_channel(channel_id);

        let received = rx.recv().await.unwrap();
        assert_eq!(received.active_channel_id, Some(channel_id));
    }

    #[test]
    fn test_chat_state_clone_shares_state() {
        let state1 = ChatState::new();
        let state2 = state1.clone();
        let channel_id = ChannelId::new();

        state1.set_active_channel(channel_id);
        assert_eq!(state2.get().active_channel_id, Some(channel_id));
    }

    // ========================================================================
    // Pinned-bar substate tests (relocated from pinned_state.rs)
    // ========================================================================
    mod pinned_tests {
        use super::super::*;

        #[test]
        fn test_pinned_state_default() {
            let state = ChatState::new();
            let data = state.get();
            assert!(data.pinned.pinned_message_ids.is_empty());
            assert_eq!(data.pinned.current_index, 0);
            assert!(!data.pinned.is_dismissed);
        }

        #[test]
        fn test_set_pinned_messages() {
            let state = ChatState::new();
            let msg1 = MessageId::new();
            let msg2 = MessageId::new();

            state.set_pinned_messages(vec![msg1, msg2]);
            let data = state.get();
            assert_eq!(data.pinned.pinned_message_ids.len(), 2);
            assert_eq!(data.pinned.current_index, 0);
        }

        #[test]
        fn test_set_pinned_messages_resets_index() {
            let state = ChatState::new();

            state.set_pinned_messages(vec![MessageId::new(), MessageId::new(), MessageId::new()]);
            state.pinned_navigate_to(2);
            assert_eq!(state.get().pinned.current_index, 2);

            // Setting new messages should reset index
            state.set_pinned_messages(vec![MessageId::new()]);
            assert_eq!(state.get().pinned.current_index, 0);
        }

        #[test]
        fn test_clear_pinned() {
            let state = ChatState::new();

            state.set_pinned_messages(vec![MessageId::new()]);
            state.pinned_dismiss();
            state.clear_pinned();

            let data = state.get();
            assert!(data.pinned.pinned_message_ids.is_empty());
            assert_eq!(data.pinned.current_index, 0);
            assert!(!data.pinned.is_dismissed);
        }

        #[test]
        fn test_navigate_next_wraparound() {
            let state = ChatState::new();
            let msg1 = MessageId::new();
            let msg2 = MessageId::new();
            let msg3 = MessageId::new();

            state.set_pinned_messages(vec![msg1, msg2, msg3]);

            assert_eq!(state.pinned_current_message_id(), Some(msg1));

            state.pinned_navigate_next();
            assert_eq!(state.pinned_current_message_id(), Some(msg2));

            state.pinned_navigate_next();
            assert_eq!(state.pinned_current_message_id(), Some(msg3));

            // Wrap around
            state.pinned_navigate_next();
            assert_eq!(state.pinned_current_message_id(), Some(msg1));
        }

        #[test]
        fn test_navigate_prev_wraparound() {
            let state = ChatState::new();
            let msg1 = MessageId::new();
            let msg2 = MessageId::new();
            let msg3 = MessageId::new();

            state.set_pinned_messages(vec![msg1, msg2, msg3]);

            // Start at first, go back to wrap to last
            state.pinned_navigate_prev();
            assert_eq!(state.pinned_current_message_id(), Some(msg3));

            state.pinned_navigate_prev();
            assert_eq!(state.pinned_current_message_id(), Some(msg2));

            state.pinned_navigate_prev();
            assert_eq!(state.pinned_current_message_id(), Some(msg1));
        }

        #[test]
        fn test_navigate_empty_list() {
            let state = ChatState::new();

            // Should not panic
            state.pinned_navigate_next();
            state.pinned_navigate_prev();
            state.pinned_navigate_to(5);

            assert!(state.pinned_current_message_id().is_none());
        }

        #[test]
        fn test_navigate_to() {
            let state = ChatState::new();
            let msg1 = MessageId::new();
            let msg2 = MessageId::new();
            let msg3 = MessageId::new();

            state.set_pinned_messages(vec![msg1, msg2, msg3]);

            state.pinned_navigate_to(2);
            assert_eq!(state.pinned_current_message_id(), Some(msg3));

            // Out of bounds - should clamp
            state.pinned_navigate_to(100);
            assert_eq!(state.pinned_current_message_id(), Some(msg3));
        }

        #[test]
        fn test_should_show() {
            let state = ChatState::new();

            // No pinned messages - should not show
            assert!(!state.pinned_should_show());

            // Has pinned messages - should show
            state.set_pinned_messages(vec![MessageId::new()]);
            assert!(state.pinned_should_show());

            // Dismissed - should not show
            state.pinned_dismiss();
            assert!(!state.pinned_should_show());

            // New channel resets dismissed
            state.set_pinned_messages(vec![MessageId::new()]);
            assert!(state.pinned_should_show());
        }

        #[test]
        fn test_current_message_id() {
            let state = ChatState::new();
            assert!(state.pinned_current_message_id().is_none());
            let msg = MessageId::new();
            state.set_pinned_messages(vec![msg]);

            assert_eq!(state.pinned_current_message_id(), Some(msg));
        }

        #[test]
        fn test_total_count() {
            let state = ChatState::new();
            assert_eq!(state.pinned_total_count(), 0);
            state.set_pinned_messages(vec![MessageId::new(), MessageId::new()]);

            assert_eq!(state.pinned_total_count(), 2);
        }

        #[test]
        fn test_add_pinned_message() {
            let state = ChatState::new();
            let msg1 = MessageId::new();
            let msg2 = MessageId::new();

            state.set_pinned_messages(vec![msg1]);
            state.add_pinned_message(msg2);

            let data = state.get();
            assert_eq!(data.pinned.pinned_message_ids.len(), 2);
            assert!(data.pinned.pinned_message_ids.contains(&msg2));

            // Adding duplicate should not add again
            state.add_pinned_message(msg1);
            assert_eq!(state.pinned_total_count(), 2);
        }

        #[test]
        fn test_remove_pinned_message() {
            let state = ChatState::new();
            let msg1 = MessageId::new();
            let msg2 = MessageId::new();
            let msg3 = MessageId::new();

            state.set_pinned_messages(vec![msg1, msg2, msg3]);
            state.pinned_navigate_to(2); // Focus msg3

            state.remove_pinned_message(&msg2);

            let data = state.get();
            assert_eq!(data.pinned.pinned_message_ids.len(), 2);
            assert!(!data.pinned.pinned_message_ids.contains(&msg2));
            // Index should still be valid (pointing to msg3, now at index 1)
            assert_eq!(data.pinned.current_index, 1);
        }

        #[test]
        fn test_remove_pinned_message_adjusts_index() {
            let state = ChatState::new();
            let msg1 = MessageId::new();
            let msg2 = MessageId::new();

            state.set_pinned_messages(vec![msg1, msg2]);
            state.pinned_navigate_to(1); // Focus msg2

            // Remove msg2 (the one at current index)
            state.remove_pinned_message(&msg2);

            let data = state.get();
            assert_eq!(data.pinned.pinned_message_ids.len(), 1);
            assert_eq!(data.pinned.current_index, 0); // Adjusted to valid range
        }

        #[test]
        fn test_remove_last_pinned_message() {
            let state = ChatState::new();
            let msg = MessageId::new();

            state.set_pinned_messages(vec![msg]);
            state.remove_pinned_message(&msg);

            let data = state.get();
            assert!(data.pinned.pinned_message_ids.is_empty());
            assert_eq!(data.pinned.current_index, 0);
        }

        #[tokio::test]
        async fn test_pinned_state_subscribe() {
            let state = ChatState::new();
            let mut rx = state.subscribe();
            let msg = MessageId::new();

            state.set_pinned_messages(vec![msg]);

            let received = rx.recv().await.unwrap();
            assert_eq!(received.pinned.pinned_message_ids.len(), 1);
        }

        #[test]
        fn test_pinned_state_clone_shares_state() {
            let state1 = ChatState::new();
            let state2 = state1.clone();

            state1.set_pinned_messages(vec![MessageId::new(), MessageId::new()]);
            assert_eq!(state2.pinned_total_count(), 2);

            state1.pinned_navigate_next();
            assert_eq!(state2.get().pinned.current_index, 1);
        }
    }

    // ========================================================================
    // Streaming substate tests (relocated from streaming_state.rs)
    // ========================================================================
    mod streaming_tests {
        use super::super::*;
        use uuid::Uuid;

        fn make_channel_id() -> ChannelId {
            ChannelId(Uuid::new_v4())
        }

        fn make_user_id() -> UserId {
            UserId(Uuid::new_v4())
        }

        fn make_message_id() -> MessageId {
            MessageId(Uuid::new_v4())
        }

        #[test]
        fn test_typing_started_older_than_stop_is_rejected() {
            let state = ChatState::new();
            let (c, u) = (make_channel_id(), make_user_id());
            assert!(state.accept_typing(c, u, 100, true));
            assert!(state.accept_typing(c, u, 200, false));
            // A STARTED sent before that STOPPED, arriving after it.
            assert!(!state.accept_typing(c, u, 150, true));
            assert!(!state.accept_typing(c, u, 200, true));
            // A late older STOPPED does not move the mark back.
            assert!(state.accept_typing(c, u, 120, false));
            assert!(!state.accept_typing(c, u, 180, true));
            assert!(state.accept_typing(c, u, 201, true));
            // Scoped per sender and per conversation; 0 = no timestamp.
            assert!(state.accept_typing(make_channel_id(), u, 150, true));
            assert!(state.accept_typing(c, make_user_id(), 150, true));
            assert!(state.accept_typing(c, u, 0, true));
        }

        // ==================== Typing Tests ====================

        #[test]
        fn test_set_and_get_typing_users() {
            let state = ChatState::new();
            let channel_id = make_channel_id();
            let user1 = make_user_id();
            let user2 = make_user_id();

            // Initially empty
            assert!(state.get_typing_users(&channel_id).is_empty());

            // Set typing users
            state.set_typing_users(channel_id.clone(), vec![user1.clone(), user2.clone()]);

            let typers = state.get_typing_users(&channel_id);
            assert_eq!(typers.len(), 2);
            assert!(typers.contains(&user1));
            assert!(typers.contains(&user2));
        }

        #[test]
        fn test_clear_typing() {
            let state = ChatState::new();
            let channel_id = make_channel_id();
            let user1 = make_user_id();

            state.set_typing_users(channel_id.clone(), vec![user1]);
            assert!(!state.get_typing_users(&channel_id).is_empty());

            state.clear_typing(&channel_id);
            assert!(state.get_typing_users(&channel_id).is_empty());
        }

        #[test]
        fn test_set_empty_typing_removes_entry() {
            let state = ChatState::new();
            let channel_id = make_channel_id();
            let user1 = make_user_id();

            state.set_typing_users(channel_id.clone(), vec![user1]);
            state.set_typing_users(channel_id.clone(), vec![]);

            // Should be removed from the map
            assert!(
                state
                    .get()
                    .streaming
                    .typing_by_channel
                    .get(&channel_id)
                    .is_none()
            );
        }

        // ==================== Reaction Tests ====================

        #[test]
        fn test_update_reaction_adds_new() {
            let state = ChatState::new();
            let message_id = make_message_id();

            // Add new reaction to message with no reactions
            state.update_reaction(&message_id, "\u{1f389}".to_string(), 1, Some(true));

            let reactions = state.get_reactions(&message_id).unwrap();
            assert_eq!(reactions.len(), 1);
            assert_eq!(reactions[0].emoji, "\u{1f389}");
            assert_eq!(reactions[0].count, 1);
            assert!(reactions[0].user_reacted);
        }

        #[test]
        fn test_update_reaction_ignores_negative_new() {
            let state = ChatState::new();
            let message_id = make_message_id();

            // Trying to decrement a non-existent reaction should do nothing
            state.update_reaction(&message_id, "\u{1f44d}".to_string(), -1, None);

            assert!(state.get_reactions(&message_id).is_none());
        }

        // ==================== General Tests ====================

        #[tokio::test]
        async fn test_subscribe_receives_typing_updates() {
            let state = ChatState::new();
            let mut rx = state.subscribe();

            let channel_id = make_channel_id();
            let user1 = make_user_id();

            state.set_typing_users(channel_id.clone(), vec![user1.clone()]);

            let received = rx.recv().await.unwrap();
            let typers = received
                .streaming
                .typing_by_channel
                .get(&channel_id)
                .unwrap();
            assert_eq!(typers.len(), 1);
            assert!(typers.contains(&user1));
        }

        #[test]
        fn test_streaming_default_impl() {
            let state = ChatState::default();
            let data = state.get();
            assert!(data.streaming.typing_by_channel.is_empty());
            assert!(data.streaming.recent_reactions.is_empty());
            assert!(data.streaming.last_typing_update.is_none());
        }

        #[test]
        fn test_streaming_clone_shares_state() {
            let state1 = ChatState::new();
            let state2 = state1.clone();

            let channel_id = make_channel_id();
            let user1 = make_user_id();

            state1.set_typing_users(channel_id.clone(), vec![user1.clone()]);

            // Both should see the update
            assert_eq!(state2.get_typing_users(&channel_id), vec![user1]);
        }
    }
}

#[cfg(test)]
mod attachment_tray_tests {
    use super::super::*;

    fn file(name: &str) -> PendingAttachment {
        PendingAttachment::new(
            AttachmentId::new(),
            name.into(),
            Default::default(),
            1,
            "image/png".into(),
        )
    }

    #[test]
    fn delayed_selection_stays_with_its_requesting_conversation() {
        let state = ChatState::new();
        let a = ActiveConversation::Channel(ChannelId::new());
        let b = ActiveConversation::ServerDm {
            peer_id: UserId::new(),
        };
        state.set_active_conversation(a);
        let revision = state.get().attachment_revision;
        state.set_active_conversation(b);
        state.add_pending_attachment(file("b.png"));
        state.add_pending_attachment_for(a, revision, file("a.png"));
        assert_eq!(state.get_pending_attachments()[0].filename, "b.png");
        state.set_active_conversation(a);
        assert_eq!(state.get_pending_attachments()[0].filename, "a.png");
        assert!(state.take_pending_attachments_for(b, revision).is_none());
        assert_eq!(
            state.take_pending_attachments_for(a, revision).unwrap()[0].filename,
            "a.png"
        );
        state.set_active_conversation(b);
        assert_eq!(state.get_pending_attachments()[0].filename, "b.png");
    }

    #[test]
    fn session_reset_clears_hidden_trays_and_rejects_late_selections() {
        let state = ChatState::new();
        let a = ActiveConversation::AnonDm(AnonDmId::new());
        state.set_active_conversation(a);
        let revision = state.get().attachment_revision;
        state.add_pending_attachment(file("private.png"));
        state.set_active_conversation(ActiveConversation::None);
        state.clear_attachment_trays();
        state.add_pending_attachment_for(a, revision, file("late.png"));
        state.set_active_conversation(a);
        assert!(state.get_pending_attachments().is_empty());
        assert!(state.get().attachment_trays.is_empty());
    }

    #[test]
    fn a_stale_take_cannot_consume_a_new_sessions_tray_in_the_same_conversation() {
        let state = ChatState::new();
        let conversation = ActiveConversation::Channel(ChannelId::new());
        state.set_active_conversation(conversation);
        let old_revision = state.get().attachment_revision;
        state.add_pending_attachment(file("old.png"));
        state.clear_attachment_trays();
        state.add_pending_attachment(file("new.png"));
        assert!(
            state
                .take_pending_attachments_for(conversation, old_revision)
                .is_none()
        );
        assert_eq!(state.get_pending_attachments()[0].filename, "new.png");
        assert_eq!(
            state
                .take_pending_attachments_for(conversation, state.get().attachment_revision)
                .unwrap()[0]
                .filename,
            "new.png",
        );
    }
}

use super::*;
use crate::domain::ids::UserId;
use crate::domain::message::{ConversationType, Message};

fn fixture() -> (tempfile::TempDir, Arc<crate::app::CoreServices>) {
    let dir = tempfile::tempdir().unwrap();
    let ctx = crate::app::AppContext::with_config(
        Some("https://localhost:1".into()),
        Some(dir.path().to_str().unwrap().into()),
    )
    .unwrap();
    let core = ctx.core_services();
    let owner = UserId::new();
    core.message_service
        .settings_repo
        .set_string(
            crate::storage::repositories::settings_repo::keys::CURRENT_USER_ID,
            &owner.to_string(),
        )
        .unwrap();
    core.auth_state.set_logged_in(owner, 1, "fixture".into());
    (dir, core)
}

#[tokio::test]
async fn malformed_cursor_is_rejected_before_mutation_during_coalescing_window() {
    let (_dir, core) = fixture();
    let service = &core.channel_stream_service;
    let channel = ChannelId::new();
    let id = MessageId::new();
    let mut recovery = RecoveryState {
        checkpoint_events: 1,
        checkpoint_written_at: Some(std::time::Instant::now()),
        candidate: Some(ValidatedGlobalCursor::parse("v1:{}".into()).unwrap()),
        ..Default::default()
    };
    let event = chat_v1::MessageEvent {
        channel_id: channel.to_string(),
        event_id: "v1:[]".into(),
        event: Some(MsgEventType::MessageSent(chat_v1::Message {
            id: id.to_string(),
            channel_id: channel.to_string(),
            author_id: UserId::new().to_string(),
            timestamp: 100,
            content: Some(chat_v1::message::Content {
                r#type: Some(chat_v1::message::content::Type::Text(
                    "must not land".into(),
                )),
            }),
            ..Default::default()
        })),
        ..Default::default()
    };
    assert!(service.handle_event(event, &mut recovery).await.is_err());
    assert!(
        core.message_service
            .message_repo
            .get(&id)
            .unwrap()
            .is_none()
    );
    assert_eq!(recovery.checkpoint_events, 1);
    assert_eq!(recovery.acknowledgeable(), Some("v1:{}"));
}

#[tokio::test]
async fn admitted_cursor_is_validated_once_across_pending_recovery_and_repeated_flushes() {
    let (_dir, core) = fixture();
    CURSOR_VALIDATIONS.with(|count| count.set(0));
    let cursor = ValidatedGlobalCursor::parse("v1:{}".into()).unwrap();
    let mut recovery = RecoveryState {
        candidate: Some(cursor),
        checkpoint_events: 1,
        ..Default::default()
    };
    recovery.defer(&ChannelId::new().to_string()).unwrap();
    core.channel_stream_service
        .flush_checkpoint(&mut recovery, true)
        .await
        .unwrap();
    assert!(
        core.message_service
            .settings_repo
            .get::<String>(CURSOR_KEY)
            .unwrap()
            .is_none()
    );
    recovery.channels.clear();
    for _ in 0..3 {
        core.channel_stream_service
            .flush_checkpoint(&mut recovery, true)
            .await
            .unwrap();
    }
    assert_eq!(
        core.message_service
            .settings_repo
            .get::<String>(CURSOR_KEY)
            .unwrap()
            .as_deref(),
        Some("v1:{}")
    );
    CURSOR_VALIDATIONS.with(|count| assert_eq!(count.get(), 1));
}

#[tokio::test]
async fn heartbeat_has_no_cursor_and_missing_payload_never_becomes_a_candidate() {
    let (_dir, core) = fixture();
    CURSOR_VALIDATIONS.with(|count| count.set(0));
    let mut recovery = RecoveryState::default();
    core.channel_stream_service
        .handle_event(
            chat_v1::MessageEvent {
                event: Some(MsgEventType::Heartbeat(Default::default())),
                ..Default::default()
            },
            &mut recovery,
        )
        .await
        .unwrap();
    CURSOR_VALIDATIONS.with(|count| assert_eq!(count.get(), 0));
    let missing = chat_v1::MessageEvent {
        channel_id: ChannelId::new().to_string(),
        event_id: "v1:{}".into(),
        ..Default::default()
    };
    assert!(
        core.channel_stream_service
            .handle_event(missing, &mut recovery)
            .await
            .is_err()
    );
    assert!(recovery.candidate.is_none());
    let bad_plan = chat_v1::MessageEvent {
        event_id: "v1:{}".into(),
        event: Some(MsgEventType::ResyncRequired(chat_v1::ResyncRequired {
            channel_ids: vec!["bad-channel".into()],
            ..Default::default()
        })),
        ..Default::default()
    };
    assert!(
        core.channel_stream_service
            .handle_event(bad_plan, &mut recovery)
            .await
            .is_err()
    );
    assert!(recovery.candidate.is_none());
    CURSOR_VALIDATIONS.with(|count| assert_eq!(count.get(), 2));
}

#[tokio::test]
async fn edit_reaction_delete_wait_off_runtime_and_checkpoint_only_after_the_write() {
    let (_dir, core) = fixture();
    let messages = &core.message_service;
    let channel = ChannelId::new();
    let author = UserId::new();
    let sent_at = crate::util::time::TimestampNanos::now().as_u64();
    let row = Message::new_text(
        channel,
        ConversationType::Channel,
        Some(author),
        "before".into(),
        sent_at,
    );
    messages.message_repo.insert(&row).unwrap();
    let updates = [
        MsgEventType::MessageUpdated(chat_v1::Message {
            id: row.id.to_string(),
            channel_id: channel.to_string(),
            author_id: author.to_string(),
            timestamp: sent_at,
            edited: true,
            edited_at: sent_at + 1,
            content: Some(chat_v1::message::Content {
                r#type: Some(chat_v1::message::content::Type::Text("after".into())),
            }),
            ..Default::default()
        }),
        MsgEventType::ReactionAdded(chat_v1::ReactionUpdate {
            target_author_aci: author.to_string(),
            target_sent_timestamp: sent_at,
            reactor_id: UserId::new().to_string(),
            emoji: "👍".into(),
            ..Default::default()
        }),
        MsgEventType::MessageDeletedId(row.id.to_string()),
    ];
    for update in updates {
        let db = messages.database.clone();
        let (ready, waiting) = std::sync::mpsc::channel();
        let (release, released) = std::sync::mpsc::channel();
        let blocker = std::thread::spawn(move || {
            let _writer = db.connection().unwrap();
            ready.send(()).unwrap();
            released.recv().unwrap();
        });
        waiting.recv().unwrap();
        let mut recovery = RecoveryState::default();
        let event = chat_v1::MessageEvent {
            channel_id: channel.to_string(),
            event_id: "v1:{}".into(),
            timestamp: sent_at + 1,
            event: Some(update),
            ..Default::default()
        };
        {
            let apply = core
                .channel_stream_service
                .handle_event(event, &mut recovery);
            tokio::pin!(apply);
            tokio::select! {
                result = &mut apply => panic!("mutation completed while writer held: {result:?}"),
                _ = tokio::time::sleep(Duration::from_millis(25)) => {}
            }
            release.send(()).unwrap();
            apply.await.unwrap();
        }
        blocker.join().unwrap();
        assert!(recovery.channels.is_empty());
        assert_eq!(recovery.checkpoint_events, 0);
    }
    assert!(messages.message_repo.get(&row.id).unwrap().is_none());
}

#[tokio::test]
async fn queued_delete_keeps_async_timer_live_and_refuses_changed_session() {
    let (_dir, core) = fixture();
    let messages = &core.message_service;
    let row = Message::new_text(
        ChannelId::new(),
        ConversationType::Channel,
        Some(UserId::new()),
        "retain".into(),
        100,
    );
    messages.message_repo.insert(&row).unwrap();
    let db = messages.database.clone();
    let (ready, waiting) = std::sync::mpsc::channel();
    let (release, released) = std::sync::mpsc::channel();
    let blocker = std::thread::spawn(move || {
        let _writer = db.connection().unwrap();
        ready.send(()).unwrap();
        released.recv().unwrap();
    });
    waiting.recv().unwrap();
    let mut recovery = RecoveryState::default();
    {
        let apply = core.channel_stream_service.handle_event(
            chat_v1::MessageEvent {
                channel_id: row.conversation_id.to_string(),
                event_id: "v1:{}".into(),
                event: Some(MsgEventType::MessageDeletedId(row.id.to_string())),
                ..Default::default()
            },
            &mut recovery,
        );
        tokio::pin!(apply);
        tokio::select! {
            result = &mut apply => panic!("delete escaped held writer: {result:?}"),
            _ = tokio::time::sleep(Duration::from_millis(25)) => {}
        }
        let owner = messages.settings_repo.current_user_id().unwrap();
        core.auth_state
            .set_logged_in(owner, 1, "new revision".into());
        release.send(()).unwrap();
        apply.await.unwrap();
    }
    blocker.join().unwrap();
    assert!(messages.message_repo.get(&row.id).unwrap().is_some());
    assert!(!messages.message_repo.is_tombstoned(&row.id).unwrap());
    assert!(!recovery.channels.is_empty());
    assert!(
        messages
            .settings_repo
            .get::<String>(CURSOR_KEY)
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn failed_checkpoint_write_retains_candidate_and_retries_without_revalidation() {
    let (_dir, core) = fixture();
    core.message_service
        .database
        .connection()
        .unwrap()
        .execute_batch(
            "CREATE TRIGGER reject_checkpoint BEFORE INSERT ON settings
         WHEN NEW.key='stream:global_events:cursor:v1'
         BEGIN SELECT RAISE(ABORT, 'checkpoint failure'); END;",
        )
        .unwrap();
    CURSOR_VALIDATIONS.with(|count| count.set(0));
    let mut recovery = RecoveryState {
        candidate: Some(ValidatedGlobalCursor::parse("v1:{}".into()).unwrap()),
        checkpoint_events: 1,
        session: Some(core.message_service.receive_session().unwrap()),
        ..Default::default()
    };
    assert!(
        core.channel_stream_service
            .flush_checkpoint(&mut recovery, true)
            .await
            .is_err()
    );
    assert_eq!(recovery.checkpoint_events, 1);
    assert!(
        core.message_service
            .settings_repo
            .get::<String>(CURSOR_KEY)
            .unwrap()
            .is_none()
    );
    core.message_service
        .database
        .connection()
        .unwrap()
        .execute_batch("DROP TRIGGER reject_checkpoint")
        .unwrap();
    core.channel_stream_service
        .flush_checkpoint(&mut recovery, true)
        .await
        .unwrap();
    assert_eq!(recovery.checkpoint_events, 0);
    CURSOR_VALIDATIONS.with(|count| assert_eq!(count.get(), 1));
}

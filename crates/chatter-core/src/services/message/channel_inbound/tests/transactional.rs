use super::*;
use crate::domain::ids::{AttachmentId, MessageId};
use crate::domain::reaction::ReactionScope;
use crate::storage::repositories::message_modifier_repo::MessageModifierRepository;

fn fixture() -> (
    tempfile::TempDir,
    Arc<ChatterClient>,
    ReceiveSession,
    Message,
) {
    let dir = tempfile::tempdir().unwrap();
    let client = ChatterClient::builder()
        .server_url("https://localhost:1")
        .data_dir(dir.path().to_str().unwrap())
        .build()
        .unwrap();
    bind_test_owner(&client);
    let session = client.messages().receive_session().unwrap();
    let row = Message::new_text(
        ChannelId::new(),
        ConversationType::Channel,
        Some(UserId::new()),
        "original".into(),
        crate::util::time::TimestampNanos::now().as_u64(),
    );
    (dir, client, session, row)
}

#[tokio::test]
async fn stale_absence_hint_merges_current_row_without_overwriting_local_changes() {
    let (_dir, client, session, original) = fixture();
    let repo = client.messages().message_repo.clone();
    assert!(repo.existing_ids(&[original.id]).unwrap().is_empty());
    // A live insertion and local mutations commit after the catch-up hint.
    repo.store_channel_received(&original, false, &session)
        .unwrap();
    let mut current = original.clone();
    current.content = MessageContent::Text("edited locally".into());
    current.is_edited = true;
    current.edited_at = Some(chrono::Utc::now());
    current.is_read = true;
    current.is_deleted = false;
    current.is_pinned = true;
    current.attachments = vec![AttachmentId::new()];
    current.mentioned_integrations = vec![crate::domain::ids::IntegrationId::new()];
    current.targets_ai_bot = true;
    current.delivery_status = "read".into();
    current.failure_reason = Some("retained".into());
    repo.update(&current).unwrap();
    let author = original.sender_id.unwrap();
    let owner = client.messages().settings_repo.current_user_id().unwrap();
    repo.add_reaction(
        &ReactionScope::of(&original),
        &author,
        original.sent_at,
        &UserId::new(),
        &owner,
        "👍",
    )
    .unwrap();
    repo.soft_delete_batch(&[original.id]).unwrap();
    let expected = repo.get(&original.id).unwrap().unwrap();
    let (inserted, stored) = repo
        .store_channel_received(&original, false, &session)
        .unwrap();
    assert!(!inserted);
    let stored = stored.unwrap();
    assert_eq!(stored.content, expected.content);
    assert_eq!(stored.edited_at, expected.edited_at);
    assert!(stored.is_edited && stored.is_read && stored.is_deleted && stored.is_pinned);
    assert_eq!(stored.attachments, expected.attachments);
    assert_eq!(
        stored.mentioned_integrations,
        expected.mentioned_integrations
    );
    assert_eq!(stored.targets_ai_bot, expected.targets_ai_bot);
    assert_eq!(stored.reaction_counts, expected.reaction_counts);
    assert_eq!(stored.delivery_status, expected.delivery_status);
    assert_eq!(stored.failure_reason, expected.failure_reason);
}

#[tokio::test]
async fn alternate_id_dedup_returns_authoritative_winner_and_rejects_identity_conflicts() {
    let (_dir, client, session, row) = fixture();
    let repo = &client.messages().message_repo;
    repo.store_channel_received(&row, false, &session).unwrap();
    let mut duplicate = row.clone();
    duplicate.id = MessageId::new();
    let (inserted, stored) = repo
        .store_channel_received(&duplicate, false, &session)
        .unwrap();
    assert!(!inserted);
    assert_eq!(stored.unwrap().id, row.id);
    assert!(repo.get(&duplicate.id).unwrap().is_none());
    duplicate.thread_id = Some(MessageId::new());
    assert!(
        repo.store_channel_received(&duplicate, false, &session)
            .is_err()
    );
    for field in 0..4 {
        let mut mismatch = row.clone();
        match field {
            0 => mismatch.conversation_id = ChannelId::new(),
            1 => mismatch.sender_id = Some(UserId::new()),
            2 => mismatch.sent_at += 1,
            _ => mismatch.conversation_type = ConversationType::AnonGroup,
        }
        assert!(
            repo.store_channel_received(&mismatch, true, &session)
                .is_err()
        );
    }
}

#[tokio::test]
async fn replay_only_fills_missing_metadata_and_never_replaces_retained_values() {
    let (_dir, client, session, row) = fixture();
    let repo = &client.messages().message_repo;
    repo.store_channel_received(&row, false, &session).unwrap();
    let mut replay = row.clone();
    replay.encrypted_content = Some(vec![1, 2]);
    replay.attachments = vec![AttachmentId::new()];
    replay.reply_to_text = Some("quote".into());
    let filled = repo
        .store_channel_received(&replay, true, &session)
        .unwrap()
        .1
        .unwrap();
    assert_eq!(filled.encrypted_content, replay.encrypted_content);
    assert_eq!(filled.attachments, replay.attachments);
    assert_eq!(filled.reply_to_text, replay.reply_to_text);
    replay.encrypted_content = Some(vec![3]);
    replay.attachments = vec![AttachmentId::new()];
    replay.reply_to_text = Some("replacement".into());
    let retained = repo
        .store_channel_received(&replay, true, &session)
        .unwrap()
        .1
        .unwrap();
    assert_eq!(retained.encrypted_content, filled.encrypted_content);
    assert_eq!(retained.attachments, filled.attachments);
    assert_eq!(retained.reply_to_text, filled.reply_to_text);
}

#[tokio::test]
async fn tombstone_after_advisory_lookup_suppresses_insert_and_receipt() {
    let (_dir, client, session, row) = fixture();
    let repo = &client.messages().message_repo;
    assert!(repo.existing_ids(&[row.id]).unwrap().is_empty());
    assert!(repo.delete(&row.id).is_err());
    let (inserted, stored) = repo.store_channel_received(&row, false, &session).unwrap();
    assert!(!inserted && stored.is_none());
    let count: i64 = client
        .messages()
        .database
        .read_connection()
        .unwrap()
        .query_row("SELECT COUNT(*) FROM channel_delivery_outbox", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(count, 0);
}

#[tokio::test]
async fn receipt_failure_rolls_back_message_and_early_modifiers_without_notification() {
    let (_dir, client, session, row) = fixture();
    let messages = client.messages();
    let modifiers = MessageModifierRepository::new(messages.database.clone());
    let now = row.sent_at;
    modifiers
        .queue_edit(
            &row.sender_id.unwrap(),
            now,
            Some(row.conversation_id),
            "early edit",
            now + 1,
            now,
        )
        .unwrap();
    modifiers
        .queue_pin(
            &row.sender_id.unwrap(),
            now,
            &client.messages().settings_repo.current_user_id().unwrap(),
            false,
            None,
            now + 1,
            now,
        )
        .unwrap();
    modifiers
        .queue_reaction(
            &row.sender_id.unwrap(),
            now,
            &client.messages().settings_repo.current_user_id().unwrap(),
            "👍",
            false,
            &ReactionScope::of(&row),
            now + 1,
            now,
        )
        .unwrap();
    messages
        .database
        .connection()
        .unwrap()
        .execute_batch(
            "CREATE TRIGGER reject_intent BEFORE INSERT ON channel_delivery_outbox
         BEGIN SELECT RAISE(ABORT, 'receipt intent failure'); END;",
        )
        .unwrap();
    let wake = messages.database.subscribe_message_stores();
    assert!(
        messages
            .message_repo
            .store_channel_received(&row, false, &session)
            .is_err()
    );
    assert!(messages.message_repo.get(&row.id).unwrap().is_none());
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(10), wake.notified())
            .await
            .is_err()
    );
    messages
        .database
        .connection()
        .unwrap()
        .execute_batch("DROP TRIGGER reject_intent")
        .unwrap();
    let stored = messages
        .message_repo
        .store_channel_received(&row, false, &session)
        .unwrap()
        .1
        .unwrap();
    assert_eq!(stored.as_text(), Some("early edit"));
    assert!(stored.is_pinned);
    assert_eq!(stored.reaction_counts.len(), 1);
    tokio::time::timeout(std::time::Duration::from_millis(50), wake.notified())
        .await
        .unwrap();
}

#[tokio::test]
async fn session_change_while_waiting_for_writer_refuses_old_receive() {
    let (_dir, client, session, row) = fixture();
    let messages = client.messages().clone();
    let writer = messages.database.connection().unwrap();
    let repo = messages.message_repo.clone();
    let id = row.id;
    let (started, waiting) = std::sync::mpsc::channel();
    let job = std::thread::spawn(move || {
        started.send(()).unwrap();
        repo.store_channel_received(&row, false, &session)
    });
    waiting.recv().unwrap();
    let owner = messages.settings_repo.current_user_id().unwrap();
    // Same owner, new revision: an owner-only check would accept this.
    client
        .auth_state()
        .set_logged_in(owner, 1, "new session".into());
    drop(writer);
    assert!(job.join().unwrap().is_err());
    assert!(messages.message_repo.get(&id).unwrap().is_none());
}

#[tokio::test]
async fn deleted_row_never_refills_erased_metadata_even_with_an_alternate_id() {
    let (_dir, client, session, row) = fixture();
    let repo = &client.messages().message_repo;
    repo.store_channel_received(&row, false, &session).unwrap();
    repo.soft_delete_batch(&[row.id]).unwrap();
    let before = repo.get(&row.id).unwrap().unwrap();
    let mut replay = row.clone();
    replay.id = MessageId::new();
    replay.encrypted_content = Some(vec![1]);
    replay.attachments = vec![AttachmentId::new()];
    replay.reply_to_text = Some("erased payload".into());
    let retained = repo
        .store_channel_received(&replay, false, &session)
        .unwrap()
        .1
        .unwrap();
    assert_eq!(retained.id, row.id);
    assert!(retained.is_deleted);
    assert_eq!(retained.encrypted_content, before.encrypted_content);
    assert_eq!(retained.attachments, before.attachments);
    assert_eq!(retained.reply_to_text, before.reply_to_text);
}

#[tokio::test]
async fn stale_hint_insert_conflict_does_not_consume_queued_modifiers() {
    let (_dir, client, session, row) = fixture();
    let messages = client.messages();
    assert!(
        messages
            .message_repo
            .existing_ids(&[row.id])
            .unwrap()
            .is_empty()
    );
    messages
        .message_repo
        .store_channel_received(&row, false, &session)
        .unwrap();
    let modifiers = MessageModifierRepository::new(messages.database.clone());
    let author = row.sender_id.unwrap();
    let owner = messages.settings_repo.current_user_id().unwrap();
    modifiers
        .queue_edit(
            &author,
            row.sent_at,
            Some(row.conversation_id),
            "queued",
            row.sent_at + 1,
            row.sent_at,
        )
        .unwrap();
    modifiers
        .queue_pin(
            &author,
            row.sent_at,
            &owner,
            false,
            None,
            row.sent_at + 1,
            row.sent_at,
        )
        .unwrap();
    modifiers
        .queue_reaction(
            &author,
            row.sent_at,
            &owner,
            "👍",
            false,
            &ReactionScope::of(&row),
            row.sent_at + 1,
            row.sent_at,
        )
        .unwrap();
    let counts = || {
        let conn = messages.database.read_connection().unwrap();
        let ordinary: i64 = conn
            .query_row("SELECT COUNT(*) FROM pending_message_modifiers", [], |r| {
                r.get(0)
            })
            .unwrap();
        let scoped: i64 = conn
            .query_row("SELECT COUNT(*) FROM pending_scoped_reactions", [], |r| {
                r.get(0)
            })
            .unwrap();
        let adds: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pending_reaction_add_selections",
                [],
                |r| r.get(0),
            )
            .unwrap();
        (ordinary, scoped, adds)
    };
    let before = counts();
    assert!(before.0 >= 2 && before.1 + before.2 > 0);
    let result = messages
        .message_repo
        .store_channel_received(&row, false, &session)
        .unwrap();
    assert!(!result.0);
    assert_eq!(
        counts(),
        before,
        "losing insert must retain every unapplied modifier"
    );
}

/// Offline diagnostic, deliberately excluded from ordinary correctness tests.
/// Traces SQL text categories only; never expands bound values or prints rows.
#[test]
#[ignore = "manual local SQL/latency measurement"]
fn measure_fifty_row_channel_pages() {
    use crate::storage::{
        Database,
        repositories::{MessageRepository, settings_repo::SettingsRepository},
    };
    use std::sync::atomic::{AtomicU64, Ordering};
    #[derive(Default)]
    struct Counts {
        statements: AtomicU64,
        full_reads: AtomicU64,
        id_batches: AtomicU64,
        commits: AtomicU64,
    }
    unsafe extern "C" fn trace(
        _: u32,
        context: *mut std::ffi::c_void,
        stmt: *mut std::ffi::c_void,
        _: *mut std::ffi::c_void,
    ) -> i32 {
        // SAFETY: the callback is installed only while the boxed Counts lives;
        // SQLITE_TRACE_STMT supplies a valid statement, and SQLite owns its SQL.
        let counts = unsafe { &*context.cast::<Counts>() };
        let sql = unsafe { rusqlite::ffi::sqlite3_sql(stmt.cast()) };
        if sql.is_null() {
            return 0;
        }
        let sql = unsafe { std::ffi::CStr::from_ptr(sql) }.to_bytes();
        counts.statements.fetch_add(1, Ordering::Relaxed);
        if sql.starts_with(b"SELECT messages.id,") {
            counts.full_reads.fetch_add(1, Ordering::Relaxed);
        }
        if sql.starts_with(b"SELECT id FROM messages WHERE id IN") {
            counts.id_batches.fetch_add(1, Ordering::Relaxed);
        }
        if sql == b"COMMIT" {
            counts.commits.fetch_add(1, Ordering::Relaxed);
        }
        0
    }
    struct Trace {
        db: Arc<Database>,
        counts: Box<Counts>,
    }
    impl Trace {
        fn install(db: Arc<Database>) -> Self {
            let mut trace_state = Self {
                db,
                counts: Box::default(),
            };
            let context = (&mut *trace_state.counts as *mut Counts).cast();
            let writer = trace_state.db.connection().unwrap();
            let first = trace_state.db.read_connection().unwrap();
            let second = trace_state.db.read_connection().unwrap();
            for conn in [&*writer, &*first, &*second] {
                // SAFETY: connections are exclusively borrowed; callback context
                // remains boxed and installed until Drop clears all three hooks.
                unsafe {
                    rusqlite::ffi::sqlite3_trace_v2(
                        conn.handle(),
                        rusqlite::ffi::SQLITE_TRACE_STMT,
                        Some(trace),
                        context,
                    );
                }
            }
            drop((writer, first, second));
            trace_state
        }
    }
    impl Drop for Trace {
        fn drop(&mut self) {
            let writer = self.db.connection().unwrap();
            let first = self.db.read_connection().unwrap();
            let second = self.db.read_connection().unwrap();
            for conn in [&*writer, &*first, &*second] {
                // SAFETY: clear hooks before the boxed callback context is freed.
                unsafe {
                    rusqlite::ffi::sqlite3_trace_v2(conn.handle(), 0, None, std::ptr::null_mut());
                }
            }
        }
    }
    for duplicates in [0, 45, 50] {
        for advisory in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let db =
                Arc::new(Database::open(&dir.path().join("measure.db"), "fixture-only").unwrap());
            db.run_migrations().unwrap();
            let owner = UserId::new();
            SettingsRepository::new(db.clone())
                .set_string(
                    crate::storage::repositories::settings_repo::keys::CURRENT_USER_ID,
                    &owner.to_string(),
                )
                .unwrap();
            let state = crate::state::AuthState::new();
            state.set_logged_in(owner, 1, "fixture".into());
            let session = ReceiveSession::capture(&state).unwrap();
            let repo = MessageRepository::new(db.clone());
            let channel = ChannelId::new();
            let author = UserId::new();
            let rows = (0..50)
                .map(|i| {
                    Message::new_text(
                        channel,
                        ConversationType::Channel,
                        Some(author),
                        "measurement payload".into(),
                        1_800_000_000_000_000_000 + i,
                    )
                })
                .collect::<Vec<_>>();
            for row in rows.iter().take(duplicates) {
                repo.store_channel_received(row, false, &session).unwrap();
            }
            let trace = Trace::install(db);
            let started = std::time::Instant::now();
            let existing = advisory.then(|| {
                repo.existing_ids(&rows.iter().map(|r| r.id).collect::<Vec<_>>())
                    .unwrap()
            });
            let mut row_times = Vec::new();
            for row in &rows {
                // Compare the same transactional merge with point-probe-first
                // scheduling versus batched advisory IDs, not historical HEAD.
                let present = existing.as_ref().is_none_or(|ids| ids.contains(&row.id));
                let row_started = std::time::Instant::now();
                repo.store_channel_received(row, present, &session).unwrap();
                row_times.push(row_started.elapsed().as_micros());
            }
            row_times.sort_unstable();
            eprintln!(
                "PAGE_MEASURE duplicates={duplicates}/50 advisory={advisory} elapsed_us={} row_p50_us={} row_p95_us={} statements={} full_message_reads={} id_batches={} commits={}",
                started.elapsed().as_micros(),
                row_times[25],
                row_times[47],
                trace.counts.statements.load(Ordering::Relaxed),
                trace.counts.full_reads.load(Ordering::Relaxed),
                trace.counts.id_batches.load(Ordering::Relaxed),
                trace.counts.commits.load(Ordering::Relaxed)
            );
        }
    }
}

#[tokio::test]
async fn queued_checkpoint_write_rejects_entry_session_after_same_owner_relogin() {
    let (_dir, client, session, _row) = fixture();
    let messages = client.messages();
    let writer = messages.database.connection().unwrap();
    let settings = messages.settings_repo.clone();
    let (started, waiting) = std::sync::mpsc::channel();
    let job = std::thread::spawn(move || {
        started.send(()).unwrap();
        settings.set_received("sync:fixture", &123u64, &session)
    });
    waiting.recv().unwrap();
    let owner = messages.settings_repo.current_user_id().unwrap();
    client
        .auth_state()
        .set_logged_in(owner, 1, "new revision".into());
    drop(writer);
    assert!(job.join().unwrap().is_err());
    assert!(
        messages
            .settings_repo
            .get::<u64>("sync:fixture")
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn local_mutation_committing_while_receive_waits_for_writer_wins_over_replay() {
    let (_dir, client, session, row) = fixture();
    let messages = client.messages();
    assert!(
        messages
            .message_repo
            .existing_ids(&[row.id])
            .unwrap()
            .is_empty()
    );
    messages
        .message_repo
        .store_channel_received(&row, false, &session)
        .unwrap();
    let writer = messages.database.connection().unwrap();
    let repo = messages.message_repo.clone();
    let replay = row.clone();
    let (started, waiting) = std::sync::mpsc::channel();
    let job = std::thread::spawn(move || {
        started.send(()).unwrap();
        repo.store_channel_received(&replay, false, &session)
    });
    waiting.recv().unwrap();
    let edited = MessageContent::Text("writer's latest edit".into());
    writer.execute("UPDATE messages SET content_json=?1,is_edited=1,is_read=1,is_pinned=1,is_deleted=1 WHERE id=?2",
        rusqlite::params![serde_json::to_string(&edited).unwrap(), row.id.to_string()]).unwrap();
    drop(writer);
    let retained = job.join().unwrap().unwrap().1.unwrap();
    assert_eq!(retained.content, edited);
    assert!(retained.is_edited && retained.is_read && retained.is_pinned && retained.is_deleted);
}

#[tokio::test]
async fn early_delete_cannot_turn_identity_collision_into_successful_suppression() {
    let (_dir, client, session, retained) = fixture();
    let messages = client.messages();
    messages
        .message_repo
        .store_channel_received(&retained, false, &session)
        .unwrap();
    let modifiers = MessageModifierRepository::new(messages.database.clone());
    for alternate in [false, true] {
        let mut incoming = retained.clone();
        if alternate {
            incoming.id = MessageId::new();
            incoming.thread_id = Some(MessageId::new());
        } else {
            incoming.sent_at += 1;
        }
        modifiers
            .queue_channel_delete(
                &incoming.sender_id.unwrap(),
                incoming.sent_at,
                incoming.conversation_id,
                retained.sent_at,
            )
            .unwrap();
        assert!(
            messages
                .message_repo
                .store_channel_received(&incoming, false, &session)
                .is_err()
        );
        assert!(!messages.message_repo.is_tombstoned(&incoming.id).unwrap());
        assert!(messages.message_repo.get(&retained.id).unwrap().is_some());
    }
}

#[tokio::test]
async fn sql_owner_change_is_rejected_even_if_auth_revision_has_not_changed() {
    let (_dir, client, session, row) = fixture();
    let messages = client.messages();
    messages
        .settings_repo
        .set_string(
            crate::storage::repositories::settings_repo::keys::CURRENT_USER_ID,
            &UserId::new().to_string(),
        )
        .unwrap();
    assert!(
        messages
            .message_repo
            .store_channel_received(&row, false, &session)
            .is_err()
    );
    assert!(messages.message_repo.get(&row.id).unwrap().is_none());
}

#[tokio::test]
async fn queued_profile_key_and_pointer_writes_refuse_expired_receive_session() {
    for pointer in [false, true] {
        let (_dir, client, session, _row) = fixture();
        let messages = client.messages();
        let owner = messages.settings_repo.current_user_id().unwrap();
        let peer = UserId::new();
        messages.user_repo.ensure_user_exists(&owner).unwrap();
        messages.user_repo.ensure_user_exists(&peer).unwrap();
        let writer = messages.database.connection().unwrap();
        let service = messages.clone();
        let (started, waiting) = std::sync::mpsc::channel();
        let job = std::thread::spawn(move || {
            started.send(()).unwrap();
            if pointer {
                crate::services::attachment::AttachmentService::new_local(
                    service.attachment_repo.clone(),
                )
                .create_from_pointers_admitted(
                    &[crate::proto::content_v1::AttachmentPointer {
                        cdn_key: "fixture-receive-pointer".into(),
                        ..Default::default()
                    }],
                    Some(&session),
                )
                .map(|_| ())
            } else {
                service
                    .user_repo
                    .set_friend_profile_key_admitted(&owner, &peer, &[7; 32], Some(&session))
                    .map(|_| ())
            }
        });
        waiting.recv().unwrap();
        client
            .auth_state()
            .set_logged_in(owner, 1, "new revision".into());
        drop(writer);
        assert!(job.join().unwrap().is_err());
        assert!(
            messages
                .user_repo
                .get_friend_profile_key(&owner, &peer)
                .unwrap()
                .is_none()
        );
        assert!(
            messages
                .attachment_repo
                .get(&AttachmentId::from_cdn_key("fixture-receive-pointer"))
                .unwrap()
                .is_none()
        );
    }
}

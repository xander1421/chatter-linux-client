use super::*;
use crate::domain::{
    ChannelId, UserId,
    message::{ConversationType, Message, ReactionCount},
    reaction::ReactionScope,
};
use crate::storage::{
    Database, MessageRepository, repositories::settings_repo::SettingsRepository,
    serialization::db_deserialize,
};
use prost::Message as _;
use std::sync::Arc;

fn open(path: &std::path::Path, own: UserId) -> Arc<Database> {
    let db = Arc::new(Database::open(path, "backup normalization fixture").unwrap());
    db.run_migrations().unwrap();
    SettingsRepository::new(db.clone())
        .set("auth.current_user_id", &own.to_string())
        .unwrap();
    db
}
fn payload(groups: &[Vec<u8>], order: &[usize], own: UserId) -> Vec<u8> {
    let mut out = Vec::new();
    push_frame(
        &mut out,
        frame::Payload::Info(BackupInfo {
            payload_version: PAYLOAD_VERSION,
            user_id: own.to_string(),
            created_at_ms: 0,
        }),
    );
    for &i in order {
        out.extend_from_slice(&groups[i]);
    }
    out
}
fn permutations(rows: &[usize]) -> Vec<Vec<usize>> {
    if rows.is_empty() {
        return vec![vec![]];
    }
    rows.iter()
        .enumerate()
        .flat_map(|(i, r)| {
            let rest = rows
                .iter()
                .enumerate()
                .filter(|(j, _)| *j != i)
                .map(|(_, r)| *r)
                .collect::<Vec<_>>();
            permutations(&rest)
                .into_iter()
                .map(move |mut p| {
                    p.insert(0, *r);
                    p
                })
                .collect::<Vec<_>>()
        })
        .collect()
}
fn groups(conn: &Connection, tables: &[&str], reverse: bool) -> Vec<Vec<u8>> {
    tables
        .iter()
        .map(|table| {
            let mut out = Vec::new();
            let mut rows = 0;
            let column = if *table == "messages" || *table == "unified_reactions" {
                "id"
            } else {
                "emoji"
            };
            dump_table(
                conn,
                &mut out,
                table,
                &format!(
                    "SELECT * FROM {table} ORDER BY {column} {}",
                    if reverse { "DESC" } else { "ASC" }
                ),
                &[],
                &mut rows,
                &|_| Ok(true),
            )
            .unwrap();
            out
        })
        .collect()
}
fn assert_projection(
    db: &Database,
    repo: &MessageRepository,
    scope: &ReactionScope,
    author: UserId,
    expected: &[&str],
    selection: Option<(&str, i64)>,
) {
    let saved = repo
        .get_by_reaction_scope(scope, &author, 100)
        .unwrap()
        .unwrap();
    assert_eq!(
        saved.current_user_reactions,
        expected.iter().map(|s| s.to_string()).collect::<Vec<_>>()
    );
    assert_eq!(
        saved.reaction_counts.iter().map(|r| r.count).sum::<u32>(),
        expected.len() as u32
    );
    assert_eq!(
        saved.content,
        crate::domain::message::MessageContent::Text("message body remains".into())
    );
    let conn = db.connection().unwrap();
    let ledger=conn.query_row("SELECT emoji,action_ts FROM reaction_actor_selections WHERE conversation_id=?1 AND conversation_type=?2 AND author_id=?3 AND target_sent_at=100",rusqlite::params![scope.id.to_string(),scope.kind.as_str(),author.to_string()],|r|Ok((r.get::<_,String>(0)?,r.get::<_,i64>(1)?))).optional().unwrap();
    assert_eq!(ledger, selection.map(|(e, c)| (e.to_string(), c)));
}
use rusqlite::OptionalExtension;

#[test]
fn reaction_backup_admits_only_the_selected_vote_in_every_frame_order_and_reexports() {
    let own = UserId::new();
    let author = UserId::new();
    let scope = ReactionScope::new(ChannelId::new(), ConversationType::Channel);
    // Two stacked clocked votes; the selection names B, so only B is admitted.
    {
        let source_dir = tempfile::tempdir().unwrap();
        let source = open(&source_dir.path().join("src.db"), own);
        let source_repo = MessageRepository::new(source.clone());
        let mut msg = Message::new_text(
            scope.id,
            scope.kind.clone(),
            Some(author),
            "message body remains".into(),
            100,
        );
        msg.current_user_reactions = vec!["A".into(), "B".into()];
        msg.reaction_counts = vec![
            ReactionCount::new("A".into(), 1),
            ReactionCount::new("B".into(), 1),
        ];
        source_repo.insert(&msg).unwrap();
        let conn = source.connection().unwrap();
        conn.execute_batch("DROP INDEX idx_ur_single_actor")
            .unwrap();
        for (id, emoji) in [("a", "A"), ("b", "B")] {
            conn.execute(
                "INSERT INTO unified_reactions VALUES(?1,?2,'channel',?3,100,?4,?5,'old')",
                rusqlite::params![
                    id,
                    scope.id.to_string(),
                    author.to_string(),
                    own.to_string(),
                    emoji
                ],
            )
            .unwrap();
        }
        {
            conn.execute("INSERT INTO reaction_action_versions VALUES(?1,'channel',?2,100,?3,'A',10,0),(?1,'channel',?2,100,?3,'B',20,0)",rusqlite::params![scope.id.to_string(),author.to_string(),own.to_string()]).unwrap();
        }
        {
            conn.execute(
                "INSERT INTO reaction_actor_selections VALUES(?1,'channel',?2,100,?3,'B',20)",
                rusqlite::params![scope.id.to_string(), author.to_string(), own.to_string()],
            )
            .unwrap();
        }
        let tables = [
            "messages",
            "unified_reactions",
            "reaction_action_versions",
            "reaction_actor_selections",
        ];
        for reverse in [false, true] {
            let sections = groups(&conn, &tables, reverse);
            for order in permutations(&(0..tables.len()).collect::<Vec<_>>()) {
                let bytes = payload(&sections, &order, own);
                let target_dir = tempfile::tempdir().unwrap();
                let target = open(&target_dir.path().join("dst.db"), own);
                import_frames(&mut target.connection().unwrap(), &bytes).unwrap();
                let repo = MessageRepository::new(target.clone());
                let expected = ["B"];
                assert_projection(
                    &target,
                    &repo,
                    &scope,
                    author,
                    &expected,
                    Some(("B", 20)),
                );
                let exported = dump_frames(&target.connection().unwrap(), &own.to_string())
                    .unwrap()
                    .0;
                assert_eq!(
                    import_frames(&mut target.connection().unwrap(), &bytes)
                        .unwrap()
                        .rows_inserted,
                    0
                );
                // Actual production exporter re-exports actor state, not merely
                // a test's desired projection.
                let again_dir = tempfile::tempdir().unwrap();
                let again = open(&again_dir.path().join("again.db"), own);
                import_frames(&mut again.connection().unwrap(), &exported).unwrap();
                let again_repo = MessageRepository::new(again.clone());
                assert_projection(
                    &again,
                    &again_repo,
                    &scope,
                    author,
                    &expected,
                    Some(("B", 20)),
                );
                assert!(
                    !again_repo
                        .add_reaction_at(&scope, &author, 100, &own, &own, "C", 15)
                        .unwrap()
                );
                assert_projection(&again, &again_repo, &scope, author, &["B"], Some(("B", 20)));
            }
        }
    }
}

#[test]
fn reaction_backup_retains_local_selected_removed_authority_exact_kinds() {
    let own = UserId::new();
    let author = UserId::new();
    let id = ChannelId::new();
    let scope = ReactionScope::new(id, ConversationType::Channel);
    let other = ReactionScope::new(id, ConversationType::ServerDm);
    let src_dir = tempfile::tempdir().unwrap();
    let src = open(&src_dir.path().join("src.db"), own);
    let repo = MessageRepository::new(src.clone());
    for s in [&scope, &other] {
        repo.insert(&Message::new_text(
            s.id,
            s.kind.clone(),
            Some(author),
            "message body remains".into(),
            100,
        ))
        .unwrap();
        repo.add_reaction_at(s, &author, 100, &own, &own, "A", 10)
            .unwrap();
    }
    let scoped = dump_frames(&src.connection().unwrap(), &own.to_string())
        .unwrap()
        .0;
    for removed in [false, true] {
        for other_local in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let db = open(&dir.path().join("local.db"), own);
            let repo = MessageRepository::new(db.clone());
            repo.insert(&Message::new_text(
                scope.id,
                scope.kind.clone(),
                Some(author),
                "message body remains".into(),
                100,
            ))
            .unwrap();
            repo.add_reaction_at(&scope, &author, 100, &own, &own, "B", 20)
                .unwrap();
            if removed {
                repo.remove_reaction_at(&scope, &author, 100, &own, &own, "B", 30)
                    .unwrap();
            }
            if other_local {
                repo.insert(&Message::new_text(
                    other.id,
                    other.kind.clone(),
                    Some(author),
                    "message body remains".into(),
                    100,
                ))
                .unwrap();
            }
            // A scoped old A also cannot clobber local B or its removed state.
            import_frames(&mut db.connection().unwrap(), &scoped).unwrap();
            let local: &[&str] = if removed { &[] } else { &["B"] };
            assert_projection(&db, &repo, &scope, author, local, Some(("B", 20)));
            assert_projection(&db, &repo, &other, author, &["A"], Some(("A", 10)));
            assert_eq!(
                import_frames(&mut db.connection().unwrap(), &scoped)
                    .unwrap()
                    .rows_inserted,
                0
            );
        }
    }
}

#[test]
fn reaction_backup_invalid_frame_rolls_back_projection_index_and_authority() {
    let own = UserId::new();
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir.path().join("rollback.db"), own);
    let repo = MessageRepository::new(db.clone());
    let scope = ReactionScope::new(ChannelId::new(), ConversationType::Channel);
    let msg = Message::new_text(
        scope.id,
        scope.kind.clone(),
        Some(own),
        "message body remains".into(),
        100,
    );
    repo.insert(&msg).unwrap();
    repo.add_reaction_at(&scope, &own, 100, &own, &own, "B", 20)
        .unwrap();
    let before: Vec<u8> = db
        .connection()
        .unwrap()
        .query_row(
            "SELECT current_user_reactions_blob FROM messages",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let mut bytes = dump_frames(&db.connection().unwrap(), &own.to_string())
        .unwrap()
        .0;
    bytes.extend_from_slice(&[255, 255, 255]);
    assert!(import_frames(&mut db.connection().unwrap(), &bytes).is_err());
    assert_projection(&db, &repo, &scope, own, &["B"], Some(("B", 20)));
    let conn = db.connection().unwrap();
    assert_eq!(
        conn.query_row(
            "SELECT current_user_reactions_blob FROM messages",
            [],
            |r| r.get::<_, Vec<u8>>(0)
        )
        .unwrap(),
        before
    );
    assert_eq!(db_deserialize::<Vec<String>>(&before).unwrap(), vec!["B"]);
    assert_eq!(
        conn.query_row(
            "SELECT count(*) FROM sqlite_master WHERE name='idx_ur_single_actor'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        1
    );
    // A valid retry after the rollback works.
    let valid = dump_frames(&conn, &own.to_string()).unwrap().0;
    drop(conn);
    import_frames(&mut db.connection().unwrap(), &valid).unwrap();
}

#[test]
fn reaction_backup_equal_clock_removal_stays_removed_in_every_frame_order() {
    let own = UserId::new();
    let author = UserId::new();
    let scope = ReactionScope::new(ChannelId::new(), ConversationType::Channel);
    {
        let dir = tempfile::tempdir().unwrap();
        let src = open(&dir.path().join("src.db"), own);
        let repo = MessageRepository::new(src.clone());
        repo.insert(&Message::new_text(
            scope.id,
            scope.kind.clone(),
            Some(author),
            "message body remains".into(),
            100,
        ))
        .unwrap();
        repo.add_reaction_at(&scope, &author, 100, &own, &own, "A", 10)
            .unwrap();
        repo.add_reaction_at(&scope, &author, 100, &own, &own, "B", 20)
            .unwrap();
        repo.remove_reaction_at(&scope, &author, 100, &own, &own, "B", 20)
            .unwrap();
        let sections = groups(
            &src.connection().unwrap(),
            &[
                "messages",
                "unified_reactions",
                "reaction_action_versions",
                "reaction_actor_selections",
            ],
            false,
        );
        for order in permutations(&[0, 1, 2, 3]) {
            let tagged = payload(&sections, &order, own);
            let dst_dir = tempfile::tempdir().unwrap();
            let dst = open(&dst_dir.path().join("dst.db"), own);
            import_frames(&mut dst.connection().unwrap(), &tagged).unwrap();
            let repo = MessageRepository::new(dst.clone());
            assert_projection(
                &dst,
                &repo,
                &scope,
                author,
                &[],
                Some(("B", 20)),
            );
            assert!(
                !repo
                    .add_reaction_at(&scope, &author, 100, &own, &own, "C", 15)
                    .unwrap()
            );
            assert_eq!(
                import_frames(&mut dst.connection().unwrap(), &tagged)
                    .unwrap()
                    .rows_inserted,
                0
            );
        }
    }
}

#[test]
fn reaction_backup_duplicate_equal_clock_versions_remove_wins_in_both_row_orders() {
    let own = UserId::new();
    let author = UserId::new();
    let scope = ReactionScope::new(ChannelId::new(), ConversationType::Channel);
    let dir = tempfile::tempdir().unwrap();
    let src = open(&dir.path().join("src.db"), own);
    let repo = MessageRepository::new(src.clone());
    repo.insert(&Message::new_text(
        scope.id,
        scope.kind.clone(),
        Some(author),
        "message body remains".into(),
        100,
    ))
    .unwrap();
    repo.add_reaction_at(&scope, &author, 100, &own, &own, "B", 20)
        .unwrap();
    let sections = groups(
        &src.connection().unwrap(),
        &[
            "messages",
            "unified_reactions",
            "reaction_action_versions",
            "reaction_actor_selections",
        ],
        false,
    );
    for reverse in [false, true] {
        let mut input = sections[2].as_slice();
        let header = Frame::decode_length_delimited(&mut input).unwrap();
        let add = Frame::decode_length_delimited(&mut input).unwrap();
        let mut remove = add.clone();
        let Some(frame::Payload::Table(ref columns)) = header.payload else {
            panic!("version header")
        };
        let Some(frame::Payload::Row(ref mut row)) = remove.payload else {
            panic!("version row")
        };
        row.values[columns.columns.iter().position(|n| n == "remove").unwrap()].kind =
            Some(column_value::Kind::Integer(1));
        let mut version_section = Vec::new();
        header
            .encode_length_delimited(&mut version_section)
            .unwrap();
        for f in if reverse {
            vec![remove, add]
        } else {
            vec![add, remove]
        } {
            f.encode_length_delimited(&mut version_section).unwrap();
        }
        let mut current = sections.clone();
        current[2] = version_section;
        for order in permutations(&[0, 1, 2, 3]) {
            let mut bytes = Vec::new();
            push_frame(
                &mut bytes,
                frame::Payload::Info(BackupInfo {
                    payload_version: PAYLOAD_VERSION,
                    user_id: own.to_string(),
                    created_at_ms: 0,
                }),
            );
            for &i in &order {
                bytes.extend_from_slice(&current[i]);
            }
            let dst_dir = tempfile::tempdir().unwrap();
            let dst = open(&dst_dir.path().join("dst.db"), own);
            import_frames(&mut dst.connection().unwrap(), &bytes).unwrap();
            let repo = MessageRepository::new(dst.clone());
            assert_projection(&dst, &repo, &scope, author, &[], Some(("B", 20)));
            assert_eq!(
                dst.connection()
                    .unwrap()
                    .query_row("SELECT remove FROM reaction_action_versions", [], |r| r
                        .get::<_, bool>(0))
                    .unwrap(),
                true
            );
            assert!(
                !repo
                    .add_reaction_at(&scope, &author, 100, &own, &own, "C", 15)
                    .unwrap()
            );
        }
    }
}

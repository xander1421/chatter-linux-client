//! Compare semantic state across ordinary, held and durable receive routes.
use super::*;
use crate::storage::{
    receive_artifact::{self, ReceiveArtifact, ReceiveKey},
    repositories::{
        message_modifier_repo::MessageModifierRepository, settings_repo::SettingsRepository,
    },
};
use prost::Message as ProstMessage;

fn permutations<T: Clone>(rows: &[T]) -> Vec<Vec<T>> {
    if rows.is_empty() {
        return vec![vec![]];
    }
    rows.iter()
        .enumerate()
        .flat_map(|(i, row)| {
            let rest: Vec<_> = rows
                .iter()
                .enumerate()
                .filter(|(j, _)| *j != i)
                .map(|(_, r)| r.clone())
                .collect();
            permutations(&rest)
                .into_iter()
                .map(move |mut order| {
                    order.insert(0, row.clone());
                    order
                })
                .collect::<Vec<_>>()
        })
        .collect()
}
fn open(path: &std::path::Path, own: UserId) -> Arc<Database> {
    let db = Arc::new(Database::open(path, "reaction storage fixture").unwrap());
    db.run_migrations().unwrap();
    let settings = SettingsRepository::new(db.clone());
    settings
        .set("auth.current_user_id", &own.to_string())
        .unwrap();
    settings.set("auth.current_device_id", &1u32).unwrap();
    db
}
fn state(db: &Database) -> (Vec<(String, i64, bool)>, Vec<(String, i64)>, Vec<String>) {
    let conn = db.connection().unwrap();
    let versions = conn
        .prepare("SELECT emoji,action_ts,remove FROM reaction_action_versions ORDER BY emoji")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    let selections = conn
        .prepare("SELECT emoji,action_ts FROM reaction_actor_selections ORDER BY emoji")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    let votes = conn
        .prepare("SELECT emoji FROM unified_reactions ORDER BY emoji")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    (versions, selections, votes)
}
fn artifact(
    db: &Database,
    scope: &ReactionScope,
    own: UserId,
    author: UserId,
    emoji: &str,
    clock: u64,
    remove: bool,
) -> ReceiveArtifact {
    use crate::proto::content_v1::{Content, Reaction, content::Content as Body};
    let content = Content {
        content: Some(Body::Reaction(Reaction {
            emoji: emoji.into(),
            target_author_aci: author.to_string(),
            target_sent_timestamp: 100,
            timestamp: clock,
            remove,
            ..Default::default()
        })),
        ..Default::default()
    };
    let artifact = ReceiveArtifact::new(
        ReceiveKey {
            local_account: own.to_string(),
            local_device: 1,
            transport: "anon_dm".into(),
            scope: scope.id.to_string(),
            envelope_id: format!("{emoji}-{clock}-{remove}"),
            server_timestamp_ns: clock,
        },
        vec![7; 32],
        own.to_string(),
        2,
        content.encode_to_vec(),
        3,
    )
    .unwrap();
    receive_artifact::insert(&db.connection().unwrap(), &artifact).unwrap();
    artifact
}

#[test]
fn reaction_storage_permutations_converge_direct_held_artifacts_reopen_and_ack() {
    let own = UserId::new();
    let author = UserId::new();
    let scope = ReactionScope::new(ChannelId::new(), ConversationType::AnonDm);
    // A delayed unrelated removal must not select A. Equal-clock B remove
    // wins in both delivery orders and still retains B's add selection.
    for actions in [
        vec![("A", 10, false), ("B", 20, false), ("A", 30, true)],
        vec![("A", 10, false), ("B", 20, false), ("B", 20, true)],
    ] {
        let mut expected = None;
        for order in permutations(&actions) {
            for mode in 0..4 {
                let dir = tempfile::tempdir().unwrap();
                let path = dir.path().join("state.db");
                let db = open(&path, own);
                let repo = UnifiedMessageRepository::new(db.clone());
                let msg = Message::new_text(
                    scope.id,
                    scope.kind.clone(),
                    Some(author),
                    "body survives".into(),
                    100,
                );
                if mode == 0 || mode == 3 {
                    repo.insert(&msg).unwrap();
                }
                let modifiers = MessageModifierRepository::new(db.clone());
                let mut artifacts = Vec::new();
                for &(emoji, clock, remove) in &order {
                    if mode == 0 {
                        if remove {
                            repo.remove_reaction_at(&scope, &author, 100, &own, &own, emoji, clock)
                                .unwrap();
                        } else {
                            repo.add_reaction_at(&scope, &author, 100, &own, &own, emoji, clock)
                                .unwrap();
                        }
                    } else if mode == 1 {
                        modifiers
                            .queue_reaction(&author, 100, &own, emoji, remove, &scope, clock, clock)
                            .unwrap();
                    } else {
                        let a = artifact(&db, &scope, own, author, emoji, clock, remove);
                        repo.apply_reaction_artifact(&a).unwrap();
                        assert!(repo.apply_reaction_artifact(&a).unwrap().is_none());
                        // Transaction B marks applied but never fabricates ACK.
                        let stored =
                            receive_artifact::load(&db.connection().unwrap(), &a.key, &a.digest)
                                .unwrap()
                                .unwrap();
                        assert!(stored.applied && !stored.acknowledged);
                        artifacts.push(a);
                    }
                }
                if mode == 1 || mode == 2 {
                    repo.insert(&msg).unwrap();
                }
                let saved = repo
                    .get_by_reaction_scope(&scope, &author, 100)
                    .unwrap()
                    .unwrap();
                assert_eq!(saved.content, msg.content);
                assert_eq!(
                    saved.current_user_reactions,
                    if actions[2].0 == "A" {
                        vec!["B".to_string()]
                    } else {
                        vec![]
                    }
                );
                assert_eq!(
                    saved.reaction_counts.iter().map(|r| r.count).sum::<u32>(),
                    saved.current_user_reactions.len() as u32
                );
                let snapshot = state(&db);
                assert_eq!(snapshot.1, vec![("B".into(), 20)]);
                if let Some(expected) = &expected {
                    assert_eq!(&snapshot, expected, "mode={mode}, order={order:?}");
                } else {
                    expected = Some(snapshot);
                }
                drop(modifiers);
                drop(repo);
                drop(db);
                let db = open(&path, own);
                let repo = UnifiedMessageRepository::new(db.clone());
                assert!(repo.replay_held_reactions().unwrap().is_empty());
                for a in artifacts {
                    assert!(repo.apply_reaction_artifact(&a).unwrap().is_none());
                    repo.acknowledge_receive_artifact(&a.key, &a.digest)
                        .unwrap();
                    repo.acknowledge_receive_artifact(&a.key, &a.digest)
                        .unwrap();
                    assert!(repo.apply_reaction_artifact(&a).unwrap().is_none());
                    assert!(
                        receive_artifact::load(&db.connection().unwrap(), &a.key, &a.digest)
                            .unwrap()
                            .unwrap()
                            .acknowledged
                    );
                }
                assert_eq!(state(&db), expected.clone().unwrap());
            }
        }
    }
}

#[test]
fn reaction_storage_equal_clock_cross_emoji_tie_and_snapshot_cannot_resurrect() {
    let own = UserId::new();
    let scope = ReactionScope::new(ChannelId::new(), ConversationType::ServerDm);
    for order in permutations(&[("A", false), ("B", false), ("B", true)]) {
        let dir = tempfile::tempdir().unwrap();
        let db = open(&dir.path().join("tie.db"), own);
        let repo = UnifiedMessageRepository::new(db.clone());
        let msg = Message::new_text(
            scope.id,
            scope.kind.clone(),
            Some(own),
            "preserved".into(),
            100,
        );
        repo.insert(&msg).unwrap();
        for (emoji, remove) in order {
            if remove {
                repo.remove_reaction_at(&scope, &own, 100, &own, &own, emoji, 20)
                    .unwrap();
            } else {
                repo.add_reaction_at(&scope, &own, 100, &own, &own, emoji, 20)
                    .unwrap();
            }
        }
        assert_eq!(state(&db).1, vec![("B".into(), 20)]);
        assert!(state(&db).2.is_empty());
        assert!(
            repo.replace_reaction_snapshot(&scope, &own, 100, &own, &[(own, "B".into())])
                .is_err()
        );
        assert!(
            repo.replace_reaction_snapshot(
                &scope,
                &own,
                100,
                &own,
                &[(own, "A".into()), (own, "B".into())]
            )
            .is_err()
        );
        assert!(
            repo.get_by_reaction_scope(&scope, &own, 100)
                .unwrap()
                .unwrap()
                .current_user_reactions
                .is_empty()
        );
    }
}

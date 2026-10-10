//! Authenticated journal-to-reaction Transaction B and bounded offline recovery.
use crate::domain::message::ConversationType;
use crate::domain::reaction::ReactionScope;
use crate::domain::{ChannelId, UserId};
use crate::error::StorageError;
use crate::inbox::DomainEvent;
use crate::storage::{MessageRepository, receive_artifact::ReceiveArtifact};
use std::sync::Arc;

pub(crate) fn group_destination_matches(actual: ChannelId, claimed: &[u8]) -> bool {
    crate::domain::anon_group::AnonGroupId::try_from_slice(claimed)
        .is_some_and(|id| ChannelId::from(id) == actual)
}
/// No author-based fallback, no payload scope authority, at most four wrappers.
pub(crate) fn decode_artifact_reaction(
    artifact: &ReceiveArtifact,
    scope: &ReactionScope,
) -> Option<(UserId, crate::proto::content_v1::Reaction)> {
    use crate::proto::content_v1::content::Content as Body;
    let mut content = crate::network::validation::protovalidate::decode_inbound_content(
        artifact.content.as_slice(),
        "reaction_journal",
    )
    .ok()?;
    for depth in 0..=4 {
        match content.content? {
            Body::Reaction(reaction) => {
                // Timestamp and emoji bounds are content.proto rules, already
                // enforced by `decode_inbound_content` (nested wrappers too).
                let author =
                    if let Some(encoded) = reaction.target_author_aci.strip_prefix("channel:") {
                        if scope.kind != ConversationType::AnonGroup {
                            return None;
                        }
                        let (id, author) = encoded.split_once(':')?;
                        if id.parse::<ChannelId>().ok()? != scope.id {
                            return None;
                        }
                        author.parse().ok()?
                    } else {
                        reaction.target_author_aci.parse().ok()?
                    };
                return Some((author, reaction));
            }
            Body::AnonDmMessage(wrapper) if scope.kind == ConversationType::AnonDm && depth < 4 => {
                let id =
                    crate::domain::anon_dm::AnonDmId::try_from_slice(&wrapper.conversation_id)?;
                if ChannelId::from(id) != scope.id {
                    return None;
                }
                content = *wrapper.inner_content?;
            }
            Body::AnonGroupMessage(wrapper)
                if scope.kind == ConversationType::AnonGroup && depth < 4 =>
            {
                if !group_destination_matches(scope.id, &wrapper.group_id) {
                    return None;
                }
                content = *wrapper.inner_content?;
            }
            _ => return None,
        }
    }
    None
}
/// Upstream success is a separate durable receipt, never an application result.
pub(crate) fn acknowledge(
    repo: &Arc<MessageRepository>,
    key: &crate::storage::receive_artifact::ReceiveKey,
    digest: &[u8],
    upstream_succeeded: bool,
) -> Result<(), StorageError> {
    if upstream_succeeded {
        repo.acknowledge_reaction_artifact(key, digest)?;
    }
    Ok(())
}
fn emit(
    applied: Option<(ReactionScope, UserId, u64)>,
    domain: Option<&tokio::sync::broadcast::Sender<DomainEvent>>,
) {
    if let (Some((scope, sender_id, sent_at_ns)), Some(domain)) = (applied, domain) {
        let _ = domain.send(DomainEvent::ReactionSnapshotChanged {
            scope,
            sender_id,
            sent_at_ns,
        });
    }
}
pub(crate) fn apply(
    repo: &Arc<MessageRepository>,
    artifact: &ReceiveArtifact,
    domain: Option<&tokio::sync::broadcast::Sender<DomainEvent>>,
) -> Result<(), StorageError> {
    emit(repo.apply_reaction_artifact(artifact)?, domain);
    Ok(())
}
pub(crate) fn replay(
    repo: &Arc<MessageRepository>,
    domain: Option<&tokio::sync::broadcast::Sender<DomainEvent>>,
) {
    // Every query is capped at 100; a full bounded journal is covered in at
    // most eleven batches. Advance over retained rows, including across failures,
    // so missing-roster heads cannot starve the 101st ready envelope on restart.
    let mut cursor = None;
    for _ in 0..11 {
        match repo.pending_reaction_artifacts_after(cursor.as_ref()) {
            Ok(artifacts) if artifacts.is_empty() => break,
            Ok(artifacts) => {
                if let Some(last) = artifacts.last() {
                    cursor = Some((last.key.server_timestamp_ns, last.id.clone()));
                }
                for artifact in artifacts {
                    if apply(repo, &artifact, domain).is_err() {
                        tracing::warn!("Reaction receive application awaits storage recovery");
                    }
                }
            }
            Err(_) => {
                tracing::debug!("Reaction receive recovery requires account/device storage");
                break;
            }
        }
    }
    match repo.replay_held_reactions() {
        Ok(applied) => {
            for entry in applied {
                emit(Some(entry), domain);
            }
        }
        Err(_) => tracing::warn!("Scoped reaction application awaits storage recovery"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::message::Message as ChatMessage;
    use crate::proto::content_v1::{self, content::Content as Body};
    use crate::storage::{
        Database,
        receive_artifact::{self, ReceiveKey},
    };
    use prost::Message;
    use rusqlite::params;

    fn open(path: &std::path::Path, own: UserId) -> (Arc<Database>, Arc<MessageRepository>) {
        let db = Arc::new(Database::open(path, "journal fixture passphrase").unwrap());
        db.run_migrations().unwrap();
        let settings =
            crate::storage::repositories::settings_repo::SettingsRepository::new(db.clone());
        settings
            .set("auth.current_user_id", &own.to_string())
            .unwrap();
        settings.set("auth.current_device_id", &1u32).unwrap();
        let repo = Arc::new(MessageRepository::new(db.clone()));
        (db, repo)
    }
    fn journal(
        db: &Database,
        own: UserId,
        scope: &ReactionScope,
        author: UserId,
        clock: u64,
        remove: bool,
        wrapped_scope: Option<ChannelId>,
    ) -> ReceiveArtifact {
        let content = content_v1::Content {
            content: Some(Body::Reaction(content_v1::Reaction {
                emoji: "👍".into(),
                target_author_aci: author.to_string(),
                target_sent_timestamp: 100,
                timestamp: clock,
                remove,
                ..Default::default()
            })),
            ..Default::default()
        };
        let content = if let Some(claim) = wrapped_scope {
            content_v1::Content {
                content: Some(match scope.kind {
                    ConversationType::AnonDm => {
                        Body::AnonDmMessage(Box::new(content_v1::AnonDmMessage {
                            conversation_id: claim.as_bytes().to_vec(),
                            inner_content: Some(Box::new(content)),
                        }))
                    }
                    _ => Body::AnonGroupMessage(Box::new(content_v1::AnonGroupMessage {
                        group_id: claim.as_bytes().to_vec(),
                        inner_content: Some(Box::new(content)),
                    })),
                }),
                ..Default::default()
            }
        } else {
            content
        };
        let artifact = ReceiveArtifact::new(
            ReceiveKey {
                local_account: own.to_string(),
                local_device: 1,
                transport: if scope.kind == ConversationType::AnonDm {
                    "anon_dm"
                } else {
                    "anon_group"
                }
                .into(),
                scope: scope.id.to_string(),
                envelope_id: format!("{}-{clock}-{remove}", scope.kind.as_str()),
                server_timestamp_ns: clock,
            },
            vec![3; 32],
            own.to_string(),
            2,
            content.encode_to_vec(),
            3,
        )
        .unwrap();
        receive_artifact::insert(&db.connection().unwrap(), &artifact).unwrap();
        artifact
    }
    fn seed(repo: &MessageRepository, scope: &ReactionScope, author: UserId) {
        repo.insert(&ChatMessage::new_text(
            scope.id,
            scope.kind.clone(),
            Some(author),
            "preserved same clock".into(),
            100,
        ))
        .unwrap();
    }
    #[test]
    fn journal_application_failure_restart_and_stale_replay_are_atomic_and_scoped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("receive.db");
        let own = UserId::new();
        let author = UserId::new();
        let id = ChannelId::new();
        let dm = ReactionScope::new(id, ConversationType::AnonDm);
        let encrypted = ReactionScope::new(id, ConversationType::AnonGroup);
        let other = ReactionScope::new(ChannelId::new(), ConversationType::AnonDm);
        let (db, repo) = open(&path, own);
        for scope in [&dm, &encrypted, &other] {
            seed(&repo, scope, author);
        }
        let a = journal(&db, own, &dm, author, 10, false, Some(id));
        db.connection().unwrap().execute_batch("CREATE TRIGGER fail_projection BEFORE UPDATE OF reaction_counts_blob ON messages BEGIN SELECT RAISE(ABORT,'fixture failure'); END;").unwrap();
        assert!(apply(&repo, &a, None).is_err());
        assert!(
            !receive_artifact::load_by_id(&db.connection().unwrap(), &a.id)
                .unwrap()
                .unwrap()
                .applied
        );
        assert!(repo.get_reactions(&dm, &author, 100).unwrap().is_empty());
        let pending: i64 = db
            .connection()
            .unwrap()
            .query_row("SELECT count(*) FROM pending_scoped_reactions", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(pending, 0, "all Transaction B effects roll back");
        db.connection()
            .unwrap()
            .execute_batch("DROP TRIGGER fail_projection")
            .unwrap();
        drop(repo);
        drop(db);
        let (db, repo) = open(&path, own);
        let (domain, mut rx) = tokio::sync::broadcast::channel(16);
        replay(&repo, Some(&domain));
        assert!(
            matches!(rx.try_recv().unwrap(),DomainEvent::ReactionSnapshotChanged {scope,..} if scope==dm)
        );
        assert!(
            repo.get_reactions(&encrypted, &author, 100)
                .unwrap()
                .is_empty()
        );
        assert!(repo.get_reactions(&other, &author, 100).unwrap().is_empty());
        assert_eq!(
            repo.get_reaction_counts(&dm, &author, 100).unwrap()[0].count,
            1
        );
        apply(&repo, &a, Some(&domain)).unwrap();
        assert!(
            rx.try_recv().is_err(),
            "duplicate artifact invents no delta or notification"
        );
        let removed = journal(&db, own, &dm, author, 20, true, None);
        apply(&repo, &removed, None).unwrap();
        let delayed = journal(&db, own, &dm, author, 15, false, None);
        apply(&repo, &delayed, None).unwrap();
        assert!(
            repo.get_reaction_counts(&dm, &author, 100)
                .unwrap()
                .is_empty(),
            "unapplied old add cannot undo newer remove"
        );
        apply(&repo, &a, None).unwrap();
        assert!(
            repo.get_reaction_counts(&dm, &author, 100)
                .unwrap()
                .is_empty()
        );
        let g = journal(&db, own, &encrypted, author, 30, false, Some(id));
        apply(&repo, &g, None).unwrap();
        assert_eq!(
            repo.get_reaction_counts(&encrypted, &author, 100).unwrap()[0].count,
            1
        );
        assert!(repo.get_reactions(&dm, &author, 100).unwrap().is_empty());
        let foreign = journal(&db, own, &dm, author, 40, false, Some(other.id));
        apply(&repo, &foreign, None).unwrap();
        assert!(repo.get_reactions(&dm, &author, 100).unwrap().is_empty());
        assert!(repo.get_reactions(&other, &author, 100).unwrap().is_empty());
        let mut forged = g.clone();
        forged.content.push(0);
        assert!(apply(&repo, &forged, None).is_err());
        db.connection()
            .unwrap()
            .execute(
                "UPDATE settings SET value=?1 WHERE key='auth.current_device_id'",
                params!["2"],
            )
            .unwrap();
        assert!(apply(&repo, &a, None).is_err());
    }
    #[test]
    fn journal_missing_target_survives_restart_and_pending_scope_is_exact() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pending.db");
        let own = UserId::new();
        let author = UserId::new();
        let scope = ReactionScope::new(ChannelId::new(), ConversationType::AnonDm);
        let collision = ReactionScope::new(scope.id, ConversationType::AnonGroup);
        let (db, repo) = open(&path, own);
        let a = journal(&db, own, &scope, author, 10, false, None);
        apply(&repo, &a, None).unwrap();
        assert!(
            receive_artifact::load_by_id(&db.connection().unwrap(), &a.id)
                .unwrap()
                .unwrap()
                .applied
        );
        drop(repo);
        drop(db);
        let (_db, repo) = open(&path, own);
        seed(&repo, &collision, author);
        assert!(
            repo.get_reactions(&collision, &author, 100)
                .unwrap()
                .is_empty()
        );
        seed(&repo, &scope, author);
        replay(&repo, None);
        assert_eq!(
            repo.get_reaction_counts(&scope, &author, 100).unwrap()[0].count,
            1
        );
        apply(&repo, &a, None).unwrap();
        assert_eq!(
            repo.get_reaction_counts(&scope, &author, 100).unwrap()[0].count,
            1
        );
    }
    fn certify_actor(
        db: &Database,
        mut artifact: ReceiveArtifact,
        actor: UserId,
    ) -> ReceiveArtifact {
        db.connection()
            .unwrap()
            .execute(
                "UPDATE reaction_receive_artifacts SET sender=?1 WHERE id=?2",
                params![actor.to_string(), artifact.id],
            )
            .unwrap();
        artifact.sender = actor.to_string();
        artifact
    }
    #[test]
    fn certified_peer_authority_and_applied_marker_failure_preserve_exact_truth() {
        use crate::domain::anon_dm::{AnonDm, AnonDmStatus};
        use crate::storage::repositories::anon_dm_repo::AnonDmRepository;
        let dir = tempfile::tempdir().unwrap();
        let own = UserId::new();
        let peer = UserId::new();
        let outsider = UserId::new();
        let author = UserId::new();
        let (db, repo) = open(&dir.path().join("authority.db"), own);
        let mut dm = AnonDm::new_initiated(peer);
        dm.status = AnonDmStatus::Active;
        dm.peer_mailbox_pk = Some([1; 32]);
        let dms = AnonDmRepository::new(db.clone());
        dms.insert_conversation(&dm).unwrap();
        let mut foreign = AnonDm::new_initiated(outsider);
        foreign.status = AnonDmStatus::Active;
        foreign.peer_mailbox_pk = Some([2; 32]);
        dms.insert_conversation(&foreign).unwrap();
        let scope = ReactionScope::new(dm.id.into(), ConversationType::AnonDm);
        let other = ReactionScope::new(foreign.id.into(), ConversationType::AnonDm);
        for scope in [&scope, &other] {
            seed(&repo, scope, author);
        }
        let a = certify_actor(
            &db,
            journal(&db, own, &scope, author, 10, false, None),
            peer,
        );
        db.connection().unwrap().execute_batch("CREATE TRIGGER fail_receipt BEFORE UPDATE OF state ON reaction_receive_artifacts BEGIN SELECT RAISE(ABORT,'receipt failure'); END;").unwrap();
        assert!(apply(&repo, &a, None).is_err());
        assert!(repo.get_reactions(&scope, &author, 100).unwrap().is_empty());
        assert!(
            !receive_artifact::load_by_id(&db.connection().unwrap(), &a.id)
                .unwrap()
                .unwrap()
                .applied
        );
        db.connection()
            .unwrap()
            .execute_batch("DROP TRIGGER fail_receipt")
            .unwrap();
        apply(&repo, &a, None).unwrap();
        assert_eq!(
            repo.get_reaction_counts(&scope, &author, 100).unwrap()[0].count,
            1
        );
        assert!(
            !repo.user_has_reacted(&scope, &author, 100, "👍").unwrap(),
            "remote actor never invents our own highlight"
        );
        let rejected = certify_actor(
            &db,
            journal(&db, own, &other, author, 20, false, None),
            peer,
        );
        apply(&repo, &rejected, None).unwrap();
        assert!(
            repo.get_reactions(&other, &author, 100).unwrap().is_empty(),
            "another active DM is not this certified peer's scope"
        );
        let r = certify_actor(&db, journal(&db, own, &scope, author, 30, true, None), peer);
        apply(&repo, &r, None).unwrap();
        assert!(
            repo.get_reaction_counts(&scope, &author, 100)
                .unwrap()
                .is_empty()
        );
        apply(&repo, &a, None).unwrap();
        assert!(
            repo.get_reaction_counts(&scope, &author, 100)
                .unwrap()
                .is_empty(),
            "already applied add cannot reverse peer's later remove"
        );
    }
    #[test]
    fn full_pending_admission_retains_journal_and_never_evicts_an_acknowledged_intent() {
        let dir = tempfile::tempdir().unwrap();
        let own = UserId::new();
        let author = UserId::new();
        let (db, repo) = open(&dir.path().join("capacity.db"), own);
        let scope = ReactionScope::new(ChannelId::new(), ConversationType::AnonDm);
        {
            let conn = db.connection().unwrap();
            let tx = conn.unchecked_transaction().unwrap();
            for target in 1..=crate::storage::repositories::message_modifier_repo::MAX_PENDING {
                tx.execute(
                    "INSERT INTO pending_scoped_reactions VALUES(?1,'anon_dm',?2,?3,?4,'👍',0,1,1)",
                    params![
                        ChannelId::new().to_string(),
                        author.to_string(),
                        target,
                        own.to_string()
                    ],
                )
                .unwrap();
            }
            tx.commit().unwrap();
        }
        let a = journal(&db, own, &scope, author, 10, false, None);
        assert!(apply(&repo, &a, None).is_err());
        assert!(
            !receive_artifact::load_by_id(&db.connection().unwrap(), &a.id)
                .unwrap()
                .unwrap()
                .applied
        );
        let count: i64 = db
            .connection()
            .unwrap()
            .query_row("SELECT count(*) FROM pending_scoped_reactions", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(count, 1000);
    }
    #[test]
    fn only_exact_confirmed_upstream_ack_compacts_receipt_and_restart_replay_stays_noop() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ack.db");
        let own = UserId::new();
        let author = UserId::new();
        let scope = ReactionScope::new(ChannelId::new(), ConversationType::AnonDm);
        let (db, repo) = open(&path, own);
        seed(&repo, &scope, author);
        let artifact = journal(&db, own, &scope, author, 10, false, None);
        assert!(
            acknowledge(&repo, &artifact.key, &artifact.digest, true).is_err(),
            "ACK never finalizes unapplied input"
        );
        apply(&repo, &artifact, None).unwrap();
        acknowledge(&repo, &artifact.key, &artifact.digest, false).unwrap();
        let stored = receive_artifact::load_by_id(&db.connection().unwrap(), &artifact.id)
            .unwrap()
            .unwrap();
        assert!(!stored.acknowledged);
        assert_eq!(stored.content, artifact.content);
        assert!(
            acknowledge(&repo, &artifact.key, &[9; 32], true).is_err(),
            "wrong-envelope digest is not an ACK receipt"
        );
        let mut wrong_time = artifact.key.clone();
        wrong_time.server_timestamp_ns += 1;
        assert!(acknowledge(&repo, &wrong_time, &artifact.digest, true).is_err());
        db.connection().unwrap().execute_batch("CREATE TRIGGER fail_ack_receipt BEFORE UPDATE OF state ON reaction_receive_artifacts BEGIN SELECT RAISE(ABORT,'ACK receipt failure'); END;").unwrap();
        assert!(acknowledge(&repo, &artifact.key, &artifact.digest, true).is_err());
        assert!(
            !receive_artifact::load_by_id(&db.connection().unwrap(), &artifact.id)
                .unwrap()
                .unwrap()
                .acknowledged
        );
        db.connection()
            .unwrap()
            .execute_batch("DROP TRIGGER fail_ack_receipt")
            .unwrap();
        acknowledge(&repo, &artifact.key, &artifact.digest, true).unwrap();
        drop(repo);
        drop(db);
        let (db, repo) = open(&path, own);
        let stored = receive_artifact::load_by_id(&db.connection().unwrap(), &artifact.id)
            .unwrap()
            .unwrap();
        assert!(stored.acknowledged);
        assert!(stored.content.is_empty());
        apply(&repo, &stored, None).unwrap();
        apply(&repo, &artifact, None).unwrap();
        assert_eq!(
            repo.get_reaction_counts(&scope, &author, 100).unwrap()[0].count,
            1
        );
    }
    #[test]
    fn encrypted_server_channel_artifact_uses_real_channel_id_and_legacy_storage_kind() {
        use crate::domain::channel::{
            Channel, ChannelType, MemberRole, Membership, NotificationPreference,
        };
        use crate::domain::user::{Friendship, FriendshipStatus};
        use crate::storage::{ChannelRepository, UserRepository};
        let dir = tempfile::tempdir().unwrap();
        let own = UserId::new();
        let peer = UserId::new();
        let author = UserId::new();
        let (db, repo) = open(&dir.path().join("encrypted-channel.db"), own);
        let users = UserRepository::new(db.clone());
        for user in [own, peer] {
            users.ensure_user_exists(&user).unwrap();
        }
        let id = ChannelId::new();
        let mut channel = Channel::new(id, "encrypted fixture".into(), ChannelType::Private, own);
        channel.config.require_encryption = true;
        let channels = ChannelRepository::new(db.clone());
        channels.insert(&channel).unwrap();
        channels
            .insert_membership(&Membership {
                channel_id: id,
                user_id: peer,
                role: MemberRole::Member,
                joined_at: chrono::Utc::now(),
                last_read_message_id: None,
                is_muted: false,
                muted_until: None,
                notification_preference: NotificationPreference::default(),
            })
            .unwrap();
        let encrypted = ReactionScope::new(id, ConversationType::AnonGroup);
        let collision = ReactionScope::new(id, ConversationType::ServerDm);
        for scope in [&encrypted, &collision] {
            seed(&repo, scope, author);
        }
        let encode = |mut artifact: ReceiveArtifact, claimed: ChannelId| {
            let mut content = content_v1::Content::decode(artifact.content.as_slice()).unwrap();
            if let Some(Body::Reaction(reaction)) = &mut content.content {
                reaction.target_author_aci = format!("channel:{claimed}:{author}");
            }
            artifact.content = content.encode_to_vec();
            db.connection()
                .unwrap()
                .execute(
                    "UPDATE reaction_receive_artifacts SET content=?1 WHERE id=?2",
                    params![artifact.content, artifact.id],
                )
                .unwrap();
            artifact
        };
        let add = encode(
            certify_actor(
                &db,
                journal(&db, own, &encrypted, author, 10, false, None),
                peer,
            ),
            id,
        );
        apply(&repo, &add, None).unwrap();
        assert_eq!(
            repo.get_reaction_counts(&encrypted, &author, 100).unwrap()[0].count,
            1
        );
        assert!(
            repo.get_reactions(&collision, &author, 100)
                .unwrap()
                .is_empty()
        );
        let remove = encode(
            certify_actor(
                &db,
                journal(&db, own, &encrypted, author, 20, true, None),
                peer,
            ),
            id,
        );
        apply(&repo, &remove, None).unwrap();
        assert!(
            repo.get_reaction_counts(&encrypted, &author, 100)
                .unwrap()
                .is_empty()
        );
        let foreign = encode(
            certify_actor(
                &db,
                journal(&db, own, &encrypted, author, 30, false, None),
                peer,
            ),
            ChannelId::new(),
        );
        apply(&repo, &foreign, None).unwrap();
        assert!(
            repo.get_reactions(&encrypted, &author, 100)
                .unwrap()
                .is_empty()
        );
        users
            .insert_friendship(&Friendship {
                requester_id: own,
                addressee_id: peer,
                status: FriendshipStatus::Blocked,
                created_at: chrono::Utc::now(),
            })
            .unwrap();
        let blocked = encode(
            certify_actor(
                &db,
                journal(&db, own, &encrypted, author, 40, false, None),
                peer,
            ),
            id,
        );
        apply(&repo, &blocked, None).unwrap();
        assert!(
            repo.get_reactions(&encrypted, &author, 100)
                .unwrap()
                .is_empty(),
            "durable artifact does not bypass Gateway's global block policy"
        );
    }
    #[test]
    fn authenticated_reactions_outpacing_dm_readiness_or_group_roster_recover_after_metadata_arrives()
     {
        use crate::domain::anon_dm::{AnonDm, AnonDmStatus};
        use crate::domain::anon_group::{AnonGroup, AnonGroupMember, AnonGroupRole};
        use crate::storage::repositories::{AnonDmRepository, AnonGroupRepository};
        let dir = tempfile::tempdir().unwrap();
        let own = UserId::new();
        let peer = UserId::new();
        let author = UserId::new();
        let (db, repo) = open(&dir.path().join("readiness.db"), own);
        let mut dm = AnonDm::new_initiated(peer);
        dm.status = AnonDmStatus::Active;
        dm.peer_mailbox_pk = Some([1; 32]);
        let group = AnonGroup::new_as_owner("delayed roster".into(), own);
        let dm_scope = ReactionScope::new(dm.id.into(), ConversationType::AnonDm);
        let group_scope = ReactionScope::new(group.id.into(), ConversationType::AnonGroup);
        for scope in [&dm_scope, &group_scope] {
            seed(&repo, scope, author);
        }
        let dm_artifact = certify_actor(
            &db,
            journal(&db, own, &dm_scope, author, 10, false, None),
            peer,
        );
        let group_artifact = certify_actor(
            &db,
            journal(&db, own, &group_scope, author, 10, false, None),
            peer,
        );
        for artifact in [&dm_artifact, &group_artifact] {
            assert!(apply(&repo, artifact, None).is_err());
            assert!(
                !receive_artifact::load_by_id(&db.connection().unwrap(), &artifact.id)
                    .unwrap()
                    .unwrap()
                    .applied
            );
        }
        let groups = AnonGroupRepository::new(db.clone());
        groups.insert_group(&group).unwrap();
        assert!(
            apply(&repo, &group_artifact, None).is_err(),
            "known group with missing roster still retains authenticated input"
        );
        let dms = AnonDmRepository::new(db.clone());
        dm.peer_mailbox_pk = None;
        dms.insert_conversation(&dm).unwrap();
        assert!(
            apply(&repo, &dm_artifact, None).is_err(),
            "active conversation without peer mailbox key must retain authenticated input"
        );
        replay(&repo, None);
        assert!(
            !receive_artifact::load_by_id(&db.connection().unwrap(), &dm_artifact.id)
                .unwrap()
                .unwrap()
                .applied
        );
        assert!(
            repo.get_reaction_counts(&dm_scope, &author, 100)
                .unwrap()
                .is_empty()
        );
        db.connection()
            .unwrap()
            .execute(
                "UPDATE anon_dm_conversations SET peer_mailbox_pk=?1 WHERE id=?2",
                rusqlite::params![[1u8; 32].as_slice(), dm.id.to_string()],
            )
            .unwrap();
        groups
            .add_member(&AnonGroupMember::new(group.id, peer, AnonGroupRole::Member))
            .unwrap();
        replay(&repo, None);
        for scope in [&dm_scope, &group_scope] {
            assert_eq!(
                repo.get_reaction_counts(scope, &author, 100).unwrap()[0].count,
                1
            );
        }
        for artifact in [&dm_artifact, &group_artifact] {
            apply(&repo, artifact, None).unwrap();
        }
    }
    #[test]
    fn fair_restart_drain_reaches_ready_artifact_after_full_readiness_held_batch() {
        use crate::domain::anon_dm::{AnonDm, AnonDmStatus};
        use crate::storage::repositories::AnonDmRepository;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fair-journal.db");
        let own = UserId::new();
        let peer = UserId::new();
        let author = UserId::new();
        let (db, repo) = open(&path, own);
        let mut dm = AnonDm::new_initiated(peer);
        dm.status = AnonDmStatus::Active;
        dm.peer_mailbox_pk = Some([1; 32]);
        let held = ReactionScope::new(dm.id.into(), ConversationType::AnonDm);
        let ready = ReactionScope::new(ChannelId::new(), ConversationType::AnonGroup);
        for scope in [&held, &ready] {
            seed(&repo, scope, author);
        }
        for clock in 1..=100 {
            certify_actor(
                &db,
                journal(&db, own, &held, author, clock, false, None),
                peer,
            );
        }
        let tail = journal(&db, own, &ready, author, 101, false, None);
        drop(repo);
        drop(db);
        let (db, repo) = open(&path, own);
        replay(&repo, None);
        assert!(
            receive_artifact::load_by_id(&db.connection().unwrap(), &tail.id)
                .unwrap()
                .unwrap()
                .applied,
            "keyset drain reaches eligible 101st item on restart"
        );
        assert_eq!(
            repo.get_reaction_counts(&ready, &author, 100).unwrap()[0].count,
            1
        );
        assert!(repo.get_reactions(&held, &author, 100).unwrap().is_empty());
        replay(&repo, None);
        assert_eq!(
            repo.get_reaction_counts(&ready, &author, 100).unwrap()[0].count,
            1
        );
        let pending: i64 = db
            .connection()
            .unwrap()
            .query_row(
                "SELECT count(*) FROM reaction_receive_artifacts WHERE state='pending'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            pending, 100,
            "held evidence was neither dropped nor marked applied"
        );
        AnonDmRepository::new(db.clone())
            .insert_conversation(&dm)
            .unwrap();
        replay(&repo, None);
        assert_eq!(
            repo.get_reaction_counts(&held, &author, 100).unwrap()[0].count,
            1
        );
        assert_eq!(
            repo.get_reaction_counts(&ready, &author, 100).unwrap()[0].count,
            1
        );
    }
    #[test]
    fn eligible_pending_target_after_thousand_ineligible_heads_is_not_starved() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fair-pending.db");
        let own = UserId::new();
        let author = UserId::new();
        let (db, repo) = open(&path, own);
        let held = ReactionScope::new(ChannelId::new(), ConversationType::AnonDm);
        let ready = ReactionScope::new(ChannelId::new(), ConversationType::AnonGroup);
        seed(&repo, &ready, author);
        {
            let conn = db.connection().unwrap();
            let tx = conn.unchecked_transaction().unwrap();
            tx.execute("WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM n WHERE x<1000) INSERT INTO messages(id,conversation_id,conversation_type,sender_id,content_type,content_json,sent_at,created_at,updated_at) SELECT printf('held-%d',x),?1,'anon_dm',?2,0,?3,x,'2026-01-01','2026-01-01' FROM n",params![held.id.to_string(),author.to_string(),serde_json::to_string(&crate::domain::message::MessageContent::Text("preserved legacy held message".into())).unwrap()]).unwrap();
            // A stranger's votes on an anon DM with no conversation row are never eligible.
            tx.execute("INSERT INTO pending_scoped_reactions SELECT conversation_id,conversation_type,sender_id,sent_at,?2,'👍',0,1,1 FROM messages WHERE conversation_id=?1",params![held.id.to_string(),UserId::new().to_string()]).unwrap();
            // Represents a restored history queue exceeding the live admission cap.
            tx.execute(
                "INSERT INTO pending_scoped_reactions VALUES(?1,'anon_group',?2,100,?3,'👍',0,1,1)",
                params![ready.id.to_string(), author.to_string(), own.to_string()],
            )
            .unwrap();
            tx.commit().unwrap();
        }
        drop(repo);
        drop(db);
        let (db, repo) = open(&path, own);
        let applied = repo.replay_held_reactions().unwrap();
        assert_eq!(applied, vec![(ready.clone(), author, 100)]);
        assert_eq!(
            repo.get_reaction_counts(&ready, &author, 100).unwrap()[0].count,
            1
        );
        assert!(repo.replay_held_reactions().unwrap().is_empty());
        let pending: i64 = db
            .connection()
            .unwrap()
            .query_row("SELECT count(*) FROM pending_scoped_reactions", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(pending, 1000, "ineligible held history remains preserved");
    }
}

//! No client/keyring/network fixture: injected delayed transport uses the actual
//! dispatch-completion helper, real repository allocator and persisted reactions.
use super::*;
use crate::domain::message::ConversationType;
use crate::domain::reaction::ReactionScope;
use crate::domain::{ChannelId, Message};
use crate::storage::{Database, MessageRepository};
use prost::Message as _;
use std::sync::Arc;

fn fixture(message: &Message) -> (tempfile::TempDir, MessageRepository) {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(Database::open(&dir.path().join("reaction.db"), "test-password").unwrap());
    db.run_migrations().unwrap();
    let repo = MessageRepository::new(db);
    repo.insert(message).unwrap();
    (dir, repo)
}

fn wire_action(emoji: &str, clock: u64, author: &UserId) -> crate::proto::content_v1::Content {
    crate::services::gateway::builders::build_reaction_content_with_channel_at(
        emoji,
        false,
        &author.to_string(),
        100,
        None,
        clock,
    )
}

fn replay(
    repo: &MessageRepository,
    scope: &ReactionScope,
    author: &UserId,
    actor: &UserId,
    content: &crate::proto::content_v1::Content,
) {
    // Copies/retries round-trip exactly the same Content plaintext.
    let decoded =
        crate::proto::content_v1::Content::decode(content.encode_to_vec().as_slice()).unwrap();
    let Some(crate::proto::content_v1::content::Content::Reaction(action)) = decoded.content else {
        panic!("reaction")
    };
    assert_eq!(decoded.timestamp, action.timestamp);
    repo.add_reaction_at(
        scope,
        author,
        100,
        actor,
        actor,
        &action.emoji,
        action.timestamp,
    )
    .unwrap();
}

#[tokio::test]
async fn delayed_dispatch_keeps_wire_clock_and_converges_for_every_scope_kind() {
    for kind in [
        ConversationType::ServerDm,
        ConversationType::Channel,
        ConversationType::AnonDm,
        ConversationType::AnonGroup,
    ] {
        let author = UserId::new();
        let actor = UserId::new();
        let message = Message::new_text(
            ChannelId::new(),
            kind.clone(),
            Some(author),
            "target".into(),
            100,
        );
        let scope = ReactionScope::of(&message);
        let (_d1, repo1) = fixture(&message);
        let (_d2, repo2) = fixture(&message);
        let first = repo1.allocate_reaction_clock(&actor).unwrap();
        let a = wire_action("A", first, &author);
        let (release, waiting) = tokio::sync::oneshot::channel();
        let send = complete_reaction_dispatch(
            first,
            async {
                waiting.await.unwrap();
                Ok(())
            },
            |same_clock| {
                assert_eq!(same_clock, a.timestamp);
                repo1
                    .add_reaction_at(&scope, &author, 100, &actor, &actor, "A", same_clock)
                    .map_err(ConversationError::Storage)
            },
        );
        let interleave = async {
            replay(&repo2, &scope, &author, &actor, &a);
            let second = repo2.allocate_reaction_clock(&actor).unwrap();
            assert!(second > first);
            let b = wire_action("B", second, &author);
            replay(&repo2, &scope, &author, &actor, &b);
            replay(&repo1, &scope, &author, &actor, &b);
            // Completion arrives after a newer linked-device action.
            release.send(()).unwrap();
            b
        };
        let (result, b) = tokio::join!(send, interleave);
        assert!(result.is_err(), "superseded dispatch must be surfaced");
        for repo in [&repo1, &repo2] {
            // Opposite event order, then duplicate retries/own-device copies.
            replay(repo, &scope, &author, &actor, &a);
            replay(repo, &scope, &author, &actor, &b);
            let stored = repo
                .get_by_reaction_scope(&scope, &author, 100)
                .unwrap()
                .unwrap();
            assert_eq!(stored.current_user_reactions, vec!["B"], "{kind:?}");
            assert_eq!(stored.reaction_counts.len(), 1);
            assert_eq!(stored.reaction_counts[0].emoji, "B");
            assert_eq!(repo.get_reactions(&scope, &author, 100).unwrap().len(), 1);
        }
    }
}

#[tokio::test]
async fn dispatch_failure_does_not_persist_and_success_receives_original_clock() {
    let recorded = std::cell::Cell::new(None);
    let failed = complete_reaction_dispatch(
        10,
        async { Err(ConversationError::Other("transport failed".into())) },
        |clock| {
            recorded.set(Some(clock));
            Ok(true)
        },
    )
    .await;
    assert!(failed.is_err());
    assert_eq!(recorded.get(), None);
    complete_reaction_dispatch(
        11,
        async {
            tokio::task::yield_now().await;
            Ok(())
        },
        |clock| {
            recorded.set(Some(clock));
            Ok(true)
        },
    )
    .await
    .unwrap();
    assert_eq!(recorded.get(), Some(11));
}

#[test]
fn concurrent_device_equal_versions_converge_with_removal_winning() {
    let author = UserId::new();
    let actor = UserId::new();
    let message = Message::new_text(
        ChannelId::new(),
        ConversationType::ServerDm,
        Some(author),
        "target".into(),
        100,
    );
    let scope = ReactionScope::of(&message);
    for reverse in [false, true] {
        let (_dir, repo) = fixture(&message);
        let mut actions = vec![("A", false), ("B", false), ("B", true)];
        if reverse {
            actions.reverse();
        }
        for (emoji, remove) in &actions {
            if *remove {
                repo.remove_reaction_at(&scope, &author, 100, &actor, &actor, emoji, 1000)
                    .unwrap();
            } else {
                repo.add_reaction_at(&scope, &author, 100, &actor, &actor, emoji, 1000)
                    .unwrap();
            }
        }
        assert!(repo.get_reactions(&scope, &author, 100).unwrap().is_empty());
        let stored = repo
            .get_by_reaction_scope(&scope, &author, 100)
            .unwrap()
            .unwrap();
        assert!(stored.current_user_reactions.is_empty());
        assert!(stored.reaction_counts.is_empty());
    }
}

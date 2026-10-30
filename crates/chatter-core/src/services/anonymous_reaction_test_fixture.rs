//! Credential-free repository fixture for the actual anonymous reaction adapters.
use crate::domain::message::ConversationType;
use crate::domain::reaction::ReactionScope;
use crate::domain::{ChannelId, Message, UserId};
use crate::services::conversation::{ConversationError, complete_reaction_dispatch};
use crate::storage::{Database, MessageRepository};
use prost::Message as _;
use std::sync::Arc;

pub(super) struct Fixture {
    _dir: tempfile::TempDir,
    pub repo: MessageRepository,
    pub scope: ReactionScope,
    pub author: UserId,
    pub actor: UserId,
}

impl Fixture {
    pub fn new(kind: ConversationType, remove: bool) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let db =
            Arc::new(Database::open(&dir.path().join("reaction.db"), "test-password").unwrap());
        db.run_migrations().unwrap();
        let repo = MessageRepository::new(db);
        let author = UserId::new();
        let actor = UserId::new();
        let message = Message::new_text(ChannelId::new(), kind, Some(author), "target".into(), 100);
        let scope = ReactionScope::of(&message);
        repo.insert(&message).unwrap();
        if remove {
            assert!(
                repo.add_reaction_at(&scope, &author, 100, &actor, &actor, "A", 10)
                    .unwrap()
            );
        }
        Self {
            _dir: dir,
            repo,
            scope,
            author,
            actor,
        }
    }

    pub async fn finish<F>(&self, dispatch: F, remove: bool, clock: u64, success: bool)
    where
        F: std::future::Future<Output = Result<(), ConversationError>>,
    {
        let result = complete_reaction_dispatch(clock, dispatch, |original| {
            assert_eq!(original, clock);
            if remove {
                self.repo.remove_reaction_at(
                    &self.scope,
                    &self.author,
                    100,
                    &self.actor,
                    &self.actor,
                    "A",
                    original,
                )
            } else {
                self.repo.add_reaction_at(
                    &self.scope,
                    &self.author,
                    100,
                    &self.actor,
                    &self.actor,
                    "A",
                    original,
                )
            }
            .map_err(ConversationError::Storage)
        })
        .await;
        assert_eq!(result.is_ok(), success);
        let selected = if success { !remove } else { remove };
        let votes = self
            .repo
            .get_reactions(&self.scope, &self.author, 100)
            .unwrap();
        assert_eq!(votes.len(), usize::from(selected));
        let message = self
            .repo
            .get_by_reaction_scope(&self.scope, &self.author, 100)
            .unwrap()
            .unwrap();
        assert_eq!(
            message.current_user_reactions,
            if selected {
                vec!["A".to_owned()]
            } else {
                vec![]
            }
        );
        assert_eq!(message.reaction_counts.len(), usize::from(selected));
        if selected {
            assert_eq!(message.reaction_counts[0].emoji, "A");
            assert_eq!(message.reaction_counts[0].count, 1);
        }
    }
}

pub(super) fn assert_encoded_action(
    content: &crate::proto::content_v1::Content,
    remove: bool,
    clock: u64,
    author: &UserId,
) {
    let decoded =
        crate::proto::content_v1::Content::decode(content.encode_to_vec().as_slice()).unwrap();
    assert_eq!(decoded.timestamp, clock);
    let Some(crate::proto::content_v1::content::Content::Reaction(reaction)) = decoded.content
    else {
        panic!("reaction");
    };
    assert_eq!(reaction.timestamp, clock);
    assert_eq!(reaction.remove, remove);
    assert_eq!(reaction.emoji, "A");
    assert_eq!(reaction.target_sent_timestamp, 100);
    assert_eq!(reaction.target_author_aci, author.to_string());
}

//! Ephemeral storage + production admission + actual event bus + local deletion.
use super::*;
use crate::domain::{
    ids::ChannelId,
    message::{ConversationType as CT, Message},
};
use crate::storage::{Database, MessageRepository};

struct Fixture {
    _dir: tempfile::TempDir,
    repo: MessageRepository,
    bus: EventBus,
    own: UserId,
}
impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(Database::open(&dir.path().join("delete.db"), "test-password").unwrap());
        db.run_migrations().unwrap();
        Self {
            _dir: dir,
            repo: MessageRepository::new(db),
            bus: EventBus::new(),
            own: UserId::new(),
        }
    }
    fn insert(&self, id: ChannelId, kind: CT, author: UserId, clock: u64) -> Message {
        let message = Message::new_text(id, kind, Some(author), "target".into(), clock);
        self.repo.insert(&message).unwrap();
        message
    }
    fn admit(
        &self,
        own: Option<UserId>,
        sender: UserId,
        id: &str,
        author: &str,
        clock: u64,
    ) -> Result<(), crate::error::StorageError> {
        admit_delete_for_me_sync(&self.repo, &self.bus, own, sender, id, author, clock)
    }
}

#[test]
fn legacy_dm_channel_and_saved_admission_deletes_only_exact_target() {
    for kind in [CT::ServerDm, CT::Channel, CT::SavedMessages] {
        let f = Fixture::new();
        let id = if kind == CT::SavedMessages {
            ChannelId::from(f.own)
        } else {
            ChannelId::new()
        };
        let author = if kind == CT::SavedMessages {
            f.own
        } else {
            UserId::new()
        };
        let target = f.insert(id, kind.clone(), author, 100);
        let survivors = [
            f.insert(ChannelId::new(), kind.clone(), author, 100),
            f.insert(id, kind.clone(), UserId::new(), 100),
            f.insert(id, kind, author, 101),
        ];
        let mut rx = f.bus.subscribe_domain();
        f.admit(
            Some(f.own),
            f.own,
            &id.to_string(),
            &author.to_string(),
            100,
        )
        .unwrap();
        let event = rx.try_recv().unwrap();
        let DomainEvent::DeleteForMeSync {
            conversation_id,
            target_author_id,
            target_message_id,
            ..
        } = event
        else {
            panic!("wrong event")
        };
        assert_eq!(conversation_id, id);
        assert_eq!(target_author_id, author);
        assert_eq!(target_message_id, target.id);
        // MessageService::delete_message_local uses this exact deletion operation.
        f.repo.delete(&target_message_id).unwrap();
        assert!(f.repo.get(&target.id).unwrap().is_none());
        for survivor in survivors {
            assert!(f.repo.get(&survivor.id).unwrap().is_some());
        }
        assert!(rx.try_recv().is_err());
    }
}

#[test]
fn malformed_foreign_missing_and_wrong_scope_never_emit_deletion() {
    let f = Fixture::new();
    let id = ChannelId::new();
    let author = UserId::new();
    let target = f.insert(id, CT::Channel, author, 100);
    let mut rx = f.bus.subscribe_domain();
    for (own, sender, cid, author_string, clock) in [
        (
            Some(f.own),
            UserId::new(),
            id.to_string(),
            author.to_string(),
            100,
        ),
        (None, f.own, id.to_string(), author.to_string(), 100),
        (
            Some(f.own),
            f.own,
            "bad cid".into(),
            author.to_string(),
            100,
        ),
        (Some(f.own), f.own, id.to_string(), "bad author".into(), 100),
        (
            Some(f.own),
            f.own,
            ChannelId::new().to_string(),
            author.to_string(),
            100,
        ),
        (
            Some(f.own),
            f.own,
            id.to_string(),
            UserId::new().to_string(),
            100,
        ),
        (Some(f.own), f.own, id.to_string(), author.to_string(), 101),
        (Some(f.own), f.own, id.to_string(), author.to_string(), 0),
        (
            Some(f.own),
            f.own,
            id.to_string(),
            author.to_string(),
            u64::MAX,
        ),
    ] {
        let _ = f.admit(own, sender, &cid, &author_string, clock);
        assert!(rx.try_recv().is_err());
        assert!(f.repo.get(&target.id).unwrap().is_some());
    }
}

#[test]
fn same_id_different_storage_kinds_refuse_even_supported_and_unsupported_collisions() {
    for other_kind in [
        CT::ServerDm,
        CT::SavedMessages,
        CT::AnonDm,
        CT::AnonGroup,
        CT::Mesh,
        CT::MeshDm,
    ] {
        let f = Fixture::new();
        let id = ChannelId::from(f.own);
        let first = f.insert(id, CT::Channel, f.own, 100);
        let second = f.insert(id, other_kind, f.own, 100);
        let mut rx = f.bus.subscribe_domain();
        assert!(f
            .admit(Some(f.own), f.own, &id.to_string(), &f.own.to_string(), 100)
            .is_err());
        assert!(rx.try_recv().is_err());
        for message in [first, second] {
            assert!(f.repo.get(&message.id).unwrap().is_some());
        }
    }
}

#[test]
fn saved_requires_own_id_and_author_and_unsupported_kinds_never_delete() {
    let f = Fixture::new();
    let mut rx = f.bus.subscribe_domain();
    for (id, kind, author) in [
        (ChannelId::new(), CT::SavedMessages, f.own),
        (ChannelId::from(f.own), CT::SavedMessages, UserId::new()),
        (ChannelId::new(), CT::AnonGroup, f.own),
        (ChannelId::new(), CT::AnonDm, f.own),
        (ChannelId::new(), CT::Mesh, f.own),
        (ChannelId::new(), CT::MeshDm, f.own),
    ] {
        let target = f.insert(id, kind, author, 100);
        assert!(f
            .admit(
                Some(f.own),
                f.own,
                &id.to_string(),
                &author.to_string(),
                100
            )
            .is_err());
        assert!(rx.try_recv().is_err());
        assert!(f.repo.get(&target.id).unwrap().is_some());
    }
}

#[test]
fn authenticated_channel_clock_resolves_only_exact_scope_and_author() {
    let f = Fixture::new();
    let id = ChannelId::new();
    let author = UserId::new();
    let target = f.insert(id, CT::Channel, author, 100);
    let mut authenticated = target.clone();
    authenticated.sent_at = 200;
    f.repo
        .bind_authenticated_channel_clock(&authenticated)
        .unwrap();
    let foreign = f.insert(ChannelId::new(), CT::Channel, author, 200);
    let mut rx = f.bus.subscribe_domain();
    f.admit(
        Some(f.own),
        f.own,
        &id.to_string(),
        &author.to_string(),
        200,
    )
    .unwrap();
    let DomainEvent::DeleteForMeSync {
        target_message_id, ..
    } = rx.try_recv().unwrap()
    else {
        panic!("wrong event")
    };
    assert_eq!(target_message_id, target.id);
    f.repo.delete(&target_message_id).unwrap();
    assert!(f.repo.get(&target.id).unwrap().is_none());
    assert!(f.repo.get(&foreign.id).unwrap().is_some());
}

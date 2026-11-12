//! Disappearing-message timers in private groups: a member's change is
//! adopted with a notice row, posts are armed by the timer they state, and
//! opening the group marks them.

use crate::client::PageAnchor;
use crate::domain::anon_group::AnonGroupRole;
use crate::domain::message::{ConversationType, MessageContent, SystemMessage, UnreadAnchor};
use crate::inbox::handlers::DomainEvent;
use crate::services::disappearing::{
    DmTimer, SecretChat, change_secret_chat_timer, secret_chat_state, stamp_timer,
    timer_update_content,
};
use crate::state::ActiveConversation;
use crate::storage::repositories::ephemeral_repo::EphemeralRepository;
use chatter_proto::content_v1::content::Content as Kind;

use super::removal::tests::fixture;

const fn timer(seconds: u32, version: u32) -> DmTimer {
    DmTimer { seconds, version, enabled_until: 0, activation_seconds: 0 }
}

/// A member's post stating `t`.
fn stamped_post(t: DmTimer, sent_at: u64) -> chatter_proto::content_v1::Content {
    let mut post = crate::services::gateway::builders::build_text_content("bye soon", sent_at);
    if let Some(Kind::DataMessage(data)) = post.content.as_mut() {
        stamp_timer(data, t);
    }
    post
}

#[tokio::test]
async fn a_members_timer_change_applies_and_files_a_visible_notice() {
    let f = fixture(AnonGroupRole::Member);
    let svc = f.client.anon_groups();
    let chat = SecretChat::Group(f.group_id);

    let events = svc
        .process_incoming_content(&f.group_id, &f.mallory, &timer_update_content(timer(30, 2), 1_000))
        .unwrap();
    assert_eq!(secret_chat_state(&svc.settings_repo, chat), timer(30, 2));
    let [DomainEvent::MessageReceived { message }] = events.as_slice() else {
        panic!("one notice event, got {events:?}");
    };
    assert_eq!(message.conversation_type, ConversationType::AnonGroup);
    assert_eq!(message.sender_id, Some(f.mallory));
    assert!(matches!(
        message.content,
        MessageContent::System(SystemMessage::DisappearingTimerChanged { seconds: 30 })
    ));
    // A secret chat shows the notice as a row; nothing else was stored.
    assert!(!crate::domain::disappearing::is_timer_notice(message));
    assert_eq!(svc.get_messages_asc(&f.group_id, None, 50).unwrap().len(), 1);

    // A stale (lower-version) update from another member is ignored.
    let stale = svc
        .process_incoming_content(&f.group_id, &f.owner, &timer_update_content(timer(0, 1), 2_000))
        .unwrap();
    assert!(stale.is_empty());
    assert_eq!(secret_chat_state(&svc.settings_repo, chat).seconds, 30);

    // Our own change bumps the version past the adopted one.
    let me = svc.current_user_id().unwrap();
    let (notice, next) = change_secret_chat_timer(
        &svc.settings_repo,
        &svc.message_service.message_repo,
        chat,
        &me,
        3_600,
        3_000,
    )
    .unwrap()
    .expect("a change");
    assert_eq!(next, timer(3_600, 3));
    assert_eq!(notice.conversation_type, ConversationType::AnonGroup);
}

#[tokio::test]
async fn an_incoming_group_post_is_armed_by_its_stated_timer() {
    let f = fixture(AnonGroupRole::Member);
    let svc = f.client.anon_groups();

    let events = svc
        .process_incoming_content(&f.group_id, &f.mallory, &stamped_post(timer(300, 2), 5_000))
        .unwrap();
    // The lost update heals from the post: notice first, then the post.
    let received: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            DomainEvent::MessageReceived { message } => Some(message.content.is_system()),
            _ => None,
        })
        .collect();
    assert_eq!(received, [true, false], "{events:?}");
    assert_eq!(
        secret_chat_state(&svc.settings_repo, SecretChat::Group(f.group_id)),
        timer(300, 2)
    );
    let post = svc
        .get_messages_asc(&f.group_id, None, 50)
        .unwrap()
        .into_iter()
        .find(|m| !m.content.is_system())
        .expect("post stored");
    let armed = EphemeralRepository::new(svc.settings_repo.database().clone())
        .get(&post.id)
        .unwrap()
        .expect("post armed");
    assert_eq!(armed.ttl.as_secs(), 300);
}

#[tokio::test]
async fn opening_a_private_group_marks_its_disappearing_rows() {
    let f = fixture(AnonGroupRole::Member);
    let svc = f.client.anon_groups();
    svc.process_incoming_content(&f.group_id, &f.mallory, &stamped_post(timer(300, 2), 5_000))
        .unwrap();

    let page = f
        .client
        .open_conversation(
            ActiveConversation::AnonGroup(f.group_id),
            PageAnchor::Opening(UnreadAnchor::default()),
            f.client.session_scope(),
        )
        .unwrap();
    let post = page
        .rows
        .iter()
        .find(|row| !row.is_system)
        .expect("post row");
    assert!(post.disappearing, "the armed post is marked on open");
}

#[tokio::test]
async fn a_new_private_group_starts_at_one_day_and_its_first_post_states_it() {
    use crate::domain::disappearing::SECRET_CHAT_DEFAULT_SECONDS;
    use crate::services::disappearing::INITIAL_TIMER_VERSION;
    let f = fixture(AnonGroupRole::Member);
    let svc = f.client.anon_groups();
    let me = svc.current_user_id().unwrap();

    // Headless: there is no server, so creation stops at mailbox
    // registration — after the group row and its timer are stored.
    let _ = svc.create_group("new".into(), Vec::new()).await;
    let group = svc
        .anon_group_repo
        .list_groups()
        .unwrap()
        .into_iter()
        .find(|g| g.id != f.group_id)
        .expect("new group stored");
    let chat = SecretChat::Group(group.id);
    let day = timer(SECRET_CHAT_DEFAULT_SECONDS, INITIAL_TIMER_VERSION);
    assert_eq!(secret_chat_state(&svc.settings_repo, chat), day);
    // The creator files the same initial notice a secret 1:1 chat does.
    let rows = svc.get_messages_asc(&group.id, None, 50).unwrap();
    assert!(
        rows.iter().any(|m| m.sender_id == Some(me)
            && matches!(
                m.content,
                MessageContent::System(SystemMessage::DisappearingTimerChanged {
                    seconds: SECRET_CHAT_DEFAULT_SECONDS
                })
            )),
        "{rows:?}"
    );

    // The first post states the day timer and its row is armed with it.
    let sent_at = 10_000;
    let mut post = crate::services::gateway::builders::build_text_content("hi", sent_at);
    let row = svc.store_text_message(&group.id, &me, "hi", sent_at).unwrap();
    svc.apply_group_timer(&group.id, &mut post, &row, sent_at).unwrap();
    let Some(Kind::DataMessage(data)) = post.content.as_ref() else {
        panic!("data message");
    };
    assert_eq!(crate::services::disappearing::stated_timer(data), Some(day));
    assert!(data.ephemeral);
    let armed = EphemeralRepository::new(svc.settings_repo.database().clone())
        .get(&row.as_str().parse().unwrap())
        .unwrap()
        .expect("post armed");
    assert_eq!(armed.ttl.as_secs(), u64::from(SECRET_CHAT_DEFAULT_SECONDS));
}

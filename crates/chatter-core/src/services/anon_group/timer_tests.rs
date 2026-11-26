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

#[tokio::test]
async fn a_join_ack_tells_the_joiner_the_groups_timer_once() {
    use crate::domain::disappearing::SECRET_CHAT_DEFAULT_SECONDS;
    use crate::services::disappearing::{
        INITIAL_TIMER_VERSION, initialize_secret_chat_timer, is_timer_update, stated_timer,
    };
    let owner = fixture(AnonGroupRole::Owner);
    let svc = owner.client.anon_groups();
    let me = svc.current_user_id().unwrap();
    // A group that never had a timer sends nothing: the joiner already agrees.
    assert!(svc.join_timer_update(&owner.group_id, 0).unwrap().is_none());

    initialize_secret_chat_timer(
        &svc.settings_repo,
        &svc.message_service.message_repo,
        SecretChat::Group(owner.group_id),
        &me,
        1_000,
    )
    .unwrap();
    let admitted_at = u64::MAX / 2;
    let update = svc
        .join_timer_update(&owner.group_id, admitted_at)
        .unwrap()
        .expect("a timer to tell");
    assert!(update.timestamp >= admitted_at, "not before the joiner's admission");
    let Some(Kind::AnonGroupMessage(wrapped)) = update.content.as_ref() else {
        panic!("a group message, got {update:?}");
    };
    assert_eq!(wrapped.group_id, owner.group_id.to_vec());
    let inner = wrapped.inner_content.as_deref().unwrap().clone();
    let Some(Kind::DataMessage(data)) = inner.content.as_ref() else {
        panic!("a data message");
    };
    assert!(is_timer_update(data));
    let day = timer(SECRET_CHAT_DEFAULT_SECONDS, INITIAL_TIMER_VERSION);
    assert_eq!(stated_timer(data), Some(day));
    // The pending queue accepts it for the joiner (it rides the JoinAck drain).
    use prost::Message as _;
    svc.anon_group_repo
        .enqueue_pending_message(&owner.group_id, &owner.mallory, &update.encode_to_vec())
        .unwrap();

    // On the joiner: adopted with one notice; a duplicate is a no-op.
    let joiner = fixture(AnonGroupRole::Member);
    let jsvc = joiner.client.anon_groups();
    let events = jsvc
        .process_incoming_content(&joiner.group_id, &joiner.owner, &inner)
        .unwrap();
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(secret_chat_state(&jsvc.settings_repo, SecretChat::Group(joiner.group_id)), day);
    let again = jsvc
        .process_incoming_content(&joiner.group_id, &joiner.owner, &inner)
        .unwrap();
    assert!(again.is_empty(), "same version: no second notice, got {again:?}");
    assert_eq!(jsvc.get_messages_asc(&joiner.group_id, None, 50).unwrap().len(), 1);
}

/// A joiner that has not yet learned the group's timer holds its post, and
/// stamps and arms it with the timer once it arrives. The ledger identity
/// ignores the stamp, so release finds the same retained row.
#[tokio::test]
async fn a_joiners_post_waits_for_the_timer_and_is_armed_at_release() {
    use crate::services::disappearing::secret_chat_timer_known;
    let f = fixture(AnonGroupRole::Member);
    let svc = f.client.anon_groups();
    let chat = SecretChat::Group(f.group_id);
    assert!(!secret_chat_timer_known(&svc.settings_repo, chat));

    let (row, sent_at) = svc.send_text(&f.group_id, "early").await.unwrap();
    let held = super::send_ledger::held_posts(&svc.message_service.database, &f.group_id.to_string()).unwrap();
    assert_eq!(held.iter().map(|(id, _)| id.as_str()).collect::<Vec<_>>(), [row.as_str()]);
    let ephemeral = EphemeralRepository::new(svc.settings_repo.database().clone());
    let id = row.as_str().parse().unwrap();
    assert!(ephemeral.get(&id).unwrap().is_none(), "nothing to arm before the timer is known");

    // Released while the timer is still unknown: it keeps waiting.
    svc.retry_held_posts_for_group(&f.group_id).await.unwrap();
    assert_eq!(super::send_ledger::held_posts(&svc.message_service.database, &f.group_id.to_string()).unwrap().len(), 1);

    svc.process_incoming_content(&f.group_id, &f.owner, &timer_update_content(timer(30, 2), 1_000))
        .unwrap();
    assert!(secret_chat_timer_known(&svc.settings_repo, chat));
    // No server here: release stamps and arms, then delivery stays pending.
    let _ = svc.retry_held_posts_for_group(&f.group_id).await;
    let armed = ephemeral.get(&id).unwrap().expect("armed at release");
    assert_eq!(armed.ttl.as_secs(), 30);

    let me = svc.current_user_id().unwrap().to_string();
    let (_, unstamped) = &held[0];
    let mut stamped = unstamped.clone();
    if let Some(Kind::DataMessage(data)) = stamped.content.as_mut() {
        stamp_timer(data, timer(30, 2));
    }
    assert_eq!(stamped.timestamp, sent_at);
    assert_eq!(
        super::send_ledger::key(&me, &f.group_id.to_string(), unstamped),
        super::send_ledger::key(&me, &f.group_id.to_string(), &stamped),
    );
}

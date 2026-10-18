use super::membership::join_is_news;
use super::stream::unannounced_invites;
use super::*;
use crate::domain::message::ConversationType;
use crate::proto::chat_v1::{
    ChannelError as ProtoChannelError, ChannelModeration, OperationFailed,
};
use crate::storage::Database;

#[test]
fn only_joins_after_the_last_sync_and_our_own_are_news() {
    let t = |secs| chrono::DateTime::from_timestamp(secs, 0).unwrap();
    let (last_sync, own) = (Some(t(100)), Some(t(50)));
    assert!(
        join_is_news(t(150), last_sync, own),
        "joined while we were away"
    );
    assert!(
        !join_is_news(t(90), last_sync, own),
        "already a member at the last sync"
    );
    assert!(
        !join_is_news(t(150), last_sync, Some(t(200))),
        "was there before we joined"
    );
    assert!(
        !join_is_news(t(150), None, own),
        "first sync backfills silently"
    );
    assert!(
        !join_is_news(t(150), last_sync, None),
        "not a member ourselves"
    );
}

#[test]
fn a_reconnect_refetch_announces_only_missed_invites() {
    let (a, b) = (ChannelId::new(), ChannelId::new());
    let mut announced = std::collections::HashSet::from([a]); // `a` came live
    let missed = unannounced_invites(&mut announced, vec![(a, ()), (b, ())]);
    assert_eq!(
        missed.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
        vec![b]
    );
    assert!(unannounced_invites(&mut announced, vec![(a, ()), (b, ())]).is_empty());

    // `a` answered, then re-sent: announced again.
    assert!(unannounced_invites(&mut announced, vec![(b, ())]).is_empty());
    assert_eq!(
        unannounced_invites(&mut announced, vec![(a, ()), (b, ())]).len(),
        1
    );
}

fn setup() -> (Arc<ChannelEventService>, EventBus, Arc<ChannelRepository>) {
    let db = crate::services::channel::membership_log::test_support::test_db();
    let (service, bus, repo, _) = setup_on(
        db,
        &crate::services::channel::membership_log::test_support::who(0xfe),
        &Default::default(),
        &[],
    );
    (service, bus, repo)
}

/// The service on `db`, its membership log signing as `me` against
/// `server`, registered as the log's observer.
fn setup_on(
    db: Arc<Database>,
    me: &crate::services::channel::membership_log::test_support::Who,
    server: &Arc<crate::services::channel::membership_log::test_support::FakeServer>,
    people: &[&crate::services::channel::membership_log::test_support::Who],
) -> (
    Arc<ChannelEventService>,
    EventBus,
    Arc<ChannelRepository>,
    Arc<MembershipLog>,
) {
    let channel_repo = Arc::new(ChannelRepository::new(db.clone()));
    let event_bus = EventBus::new();
    // Never reachable: the rollback fetch is expected to fail, which is
    // the "server no longer has this channel" path.
    let grpc_pool = Arc::new(
        crate::network::grpc::client_pool::GrpcClientPool::new("https://127.0.0.1:1").unwrap(),
    );
    let (log, _, _) = crate::services::channel::membership_log::test_support::device_on(
        db.clone(),
        me,
        server,
        people,
    );

    let service = Arc::new(ChannelEventService::new(
        Arc::new(MessageRepository::new(db.clone())),
        channel_repo.clone(),
        Arc::new(UserRepository::new(db.clone())),
        grpc_pool,
        event_bus.clone(),
        Arc::new(crate::storage::repositories::settings_repo::SettingsRepository::new(db)),
        log.clone(),
    ));
    log.set_observer(Arc::downgrade(&(service.clone() as Arc<dyn LogObserver>)));

    (service, event_bus, channel_repo, log)
}

/// The keepalive changes nothing and says nothing, even with the empty
/// channel_id an all-channels subscription sends it with.
#[tokio::test]
async fn a_heartbeat_changes_nothing() {
    let (service, bus, _) = setup();
    let mut rx = bus.domain_sender().subscribe();
    service
        .handle_stream_event(
            channel_event::Event::Heartbeat(crate::proto::chat_v1::StreamHeartbeat {}),
            "",
        )
        .await;
    assert!(
        rx.try_recv().is_err(),
        "a heartbeat produced a domain event"
    );
}

/// INFRA-29 regression: `OperationFailed` is the saga-compensation signal.
/// It used to fall into `_ => debug!("Unhandled channel event type")`, so a
/// Temporal channel workflow could roll back and the client would keep its
/// optimistic state forever, with nothing on screen and nothing in the log.
#[tokio::test]
async fn operation_failed_reaches_the_ui_layer() {
    let (service, bus, _) = setup();
    let mut rx = bus.domain_sender().subscribe();
    let channel_id = ChannelId::from(uuid::Uuid::new_v4());

    service
        .handle_stream_event(
            channel_event::Event::OperationFailed(OperationFailed {
                operation_id: "op-123".to_string(),
                channel_id: channel_id.to_string(),
                user_id: uuid::Uuid::new_v4().to_string(),
                error: Some(ProtoChannelError {
                    code: crate::proto::chat_v1::channel_error::Code::PermissionDenied as i32,
                    message: "not allowed".to_string(),
                    details: Default::default(),
                }),
            }),
            &channel_id.to_string(),
        )
        .await;

    match rx.try_recv() {
        Ok(DomainEvent::ChannelOperationFailed {
            channel_id: got,
            operation_id,
            code,
            message,
        }) => {
            assert_eq!(got, channel_id);
            assert_eq!(operation_id, "op-123");
            assert_eq!(code, "PERMISSION_DENIED");
            assert_eq!(message, "not allowed");
        }
        other => panic!("OperationFailed did not reach the domain bus: {:?}", other),
    }
}

/// A deleted channel stays listed read-only (Signal keeps an ended group):
/// our membership goes, the channel is marked deleted, one "{owner}
/// deleted the channel" is filed however often the deletion is heard, and
/// the UI is told as for any departure of ours.
#[tokio::test]
async fn channel_deleted_keeps_the_channel_read_only_with_one_notice() {
    let (service, bus, channel_repo) = setup();
    let mut rx = bus.domain_sender().subscribe();
    let me = as_me(&service);
    let channel_id = private_channel(&channel_repo);
    let own = Membership {
        channel_id,
        user_id: me,
        role: crate::domain::channel::MemberRole::Member,
        notification_preference: crate::domain::channel::NotificationPreference::All,
        joined_at: Utc::now(),
        last_read_message_id: None,
        is_muted: false,
        muted_until: None,
    };
    channel_repo.insert_or_ignore_membership(&own).unwrap();

    let deleted = || channel_event::Event::ChannelDeleted(channel_id.to_string());
    service
        .handle_stream_event(deleted(), &channel_id.to_string())
        .await;
    service.record_channel_deleted(&channel_id); // the sync finding it too

    assert!(
        channel_repo.get(&channel_id).unwrap().is_some(),
        "history must stay"
    );
    assert!(
        channel_repo
            .get_membership(&channel_id, &me)
            .unwrap()
            .is_none()
    );
    assert_eq!(channel_repo.former_state(&channel_id).unwrap(), Some(true));
    assert_eq!(
        channel_repo.list_former_for_user(&me).unwrap().len(),
        1,
        "not listed read-only"
    );
    assert_eq!(
        service
            .message_repo
            .count(&channel_id, &ConversationType::Channel)
            .unwrap(),
        1
    );
    match rx.try_recv() {
        Ok(DomainEvent::ChannelMemberLeft {
            user_id, message, ..
        }) => {
            assert_eq!(user_id, me);
            assert!(matches!(
                message.content,
                MessageContent::System(SystemMessage::ChannelDeleted)
            ));
        }
        other => panic!("expected ChannelMemberLeft, got {other:?}"),
    }
    assert!(rx.try_recv().is_err(), "the second hearing reached the UI");

    // A later leave notice for the same channel does not undo "deleted".
    channel_repo.mark_former(&channel_id, false).unwrap();
    assert_eq!(channel_repo.former_state(&channel_id).unwrap(), Some(true));

    // A channel never stored here has nothing to keep listed.
    let unknown = ChannelId::from(uuid::Uuid::new_v4());
    channel_repo.mark_former(&unknown, false).unwrap();
    assert_eq!(channel_repo.former_state(&unknown).unwrap(), None);
}

use crate::proto::chat_v1::{
    self as pb, channel_action::Action, remove_member::Reason as RmReason,
};
use crate::services::channel::membership_log::AuthorBase;
use crate::services::channel::membership_log::test_support::{FakeServer, Who, who};
use crate::services::channel::verified_state::{ChannelRole, Policy};

fn act(a: Action) -> pb::ChannelAction {
    pb::ChannelAction { action: Some(a) }
}

/// A private channel owned by `owner` on a fake server, and this
/// device's event service logged in as `me`, an ACTIVE member.
struct World {
    server: Arc<FakeServer>,
    owner: Who,
    me: Who,
    other: Who,
    o_log: Arc<MembershipLog>,
    my_log: Arc<MembershipLog>,
    service: Arc<ChannelEventService>,
    bus: EventBus,
    channel_repo: Arc<ChannelRepository>,
    ch: ChannelId,
}

impl World {
    async fn new() -> Self {
        let (owner, me, other) = (who(1), who(2), who(3));
        let people = [&owner, &me, &other];
        let server = Arc::new(FakeServer::default());
        let db = crate::services::channel::membership_log::test_support::test_db();
        let (service, bus, channel_repo, my_log) = setup_on(db, &me, &server, &people);
        service
            .settings_repo
            .set_string(
                crate::storage::repositories::settings_repo::keys::CURRENT_USER_ID,
                &me.id,
            )
            .unwrap();
        let (o_log, _, _) = crate::services::channel::membership_log::test_support::device(
            &owner, &server, &people,
        );
        let ch = ChannelId::from(uuid::Uuid::new_v4());
        let channel = crate::domain::channel::Channel::new(
            ch,
            "verified".into(),
            crate::domain::channel::ChannelType::Private,
            UserId::from(owner.id.parse::<uuid::Uuid>().unwrap()),
        );
        channel_repo.upsert(&channel).unwrap();
        let w = World {
            server,
            owner,
            me,
            other,
            o_log,
            my_log,
            service,
            bus,
            channel_repo,
            ch,
        };
        w.by_owner(vec![act(Action::Genesis(pb::Genesis {
            policy: Some(Policy::defaults(false).to_proto()),
            disappearing_seconds: 0,
        }))])
        .await;
        w.by_owner(vec![w.add(&w.me)]).await;
        w.relay_last().await;
        w.author(
            &w.my_log,
            vec![act(Action::AcceptInvite(pb::AcceptInvite {}))],
        )
        .await;
        w
    }

    fn add(&self, who: &Who) -> pb::ChannelAction {
        act(Action::AddMember(pb::AddMember {
            user_id: who.id.clone(),
            role: ChannelRole::Member as i32,
            identity_key: who.key().to_vec(),
            pending: true,
        }))
    }

    async fn author(&self, log: &MembershipLog, acts: Vec<pb::ChannelAction>) {
        let base = if acts
            .iter()
            .any(|a| matches!(a.action, Some(Action::Genesis(_))))
        {
            AuthorBase::Genesis
        } else {
            AuthorBase::Verified
        };
        log.author(
            &self.ch,
            base,
            None,
            |_| Ok(acts.clone()),
            |c| {
                let r = self.server.append(&self.ch, c);
                async move { r }
            },
        )
        .await
        .unwrap();
    }

    async fn by_owner(&self, acts: Vec<pb::ChannelAction>) {
        self.o_log.catch_up(&self.ch).await.unwrap();
        self.author(&self.o_log, acts).await;
    }

    fn last(&self) -> pb::ChannelChange {
        self.server.log.lock().last().unwrap().clone()
    }

    async fn relay(&self, change: pb::ChannelChange) {
        self.service
            .handle_stream_event(
                channel_event::Event::ChannelChange(change),
                &self.ch.to_string(),
            )
            .await;
    }

    async fn relay_last(&self) {
        self.relay(self.last()).await;
    }

    fn count(&self) -> u64 {
        self.service
            .message_repo
            .count(&self.ch, &ConversationType::Channel)
            .unwrap()
    }

    fn uid(w: &Who) -> UserId {
        UserId::from(w.id.parse::<uuid::Uuid>().unwrap())
    }
}

/// F77 on the verified path: the server relays one change on two paths
/// (channel stream + per-user stream). It is applied once, files one
/// notice and tells the UI once; a later change is a new notice.
#[tokio::test]
async fn a_membership_change_delivered_twice_files_one_notice() {
    let w = World::new().await;
    let mut rx = w.bus.domain_sender().subscribe();
    let base = w.count();

    // Other is invited (no notice) and accepts: "joined".
    w.by_owner(vec![w.add(&w.other)]).await;
    let (x_log, _, _) = crate::services::channel::membership_log::test_support::device(
        &w.other,
        &w.server,
        &[&w.owner, &w.me, &w.other],
    );
    w.author(&x_log, vec![act(Action::AcceptInvite(pb::AcceptInvite {}))])
        .await;
    let accept = w.last();
    for _ in 0..2 {
        w.relay(accept.clone()).await;
    }
    assert_eq!(w.count(), base + 1, "one join filed two notices");
    assert!(matches!(
        rx.try_recv(),
        Ok(DomainEvent::ChannelMemberJoined { .. })
    ));
    assert!(rx.try_recv().is_err(), "the duplicate reached the UI");
    assert!(
        w.channel_repo
            .get_membership(&w.ch, &World::uid(&w.other))
            .unwrap()
            .is_some()
    );

    // Other leaves: "left", once.
    w.author(
        &x_log,
        vec![act(Action::RemoveMember(pb::RemoveMember {
            user_id: w.other.id.clone(),
            reason: RmReason::Left as i32,
        }))],
    )
    .await;
    let leave = w.last();
    for _ in 0..2 {
        w.relay(leave.clone()).await;
    }
    assert_eq!(w.count(), base + 2, "one leave filed two notices");
    assert!(matches!(
        rx.try_recv(),
        Ok(DomainEvent::ChannelMemberLeft { .. })
    ));
    assert!(rx.try_recv().is_err(), "the duplicate reached the UI");
    assert!(
        w.channel_repo
            .get_membership(&w.ch, &World::uid(&w.other))
            .unwrap()
            .is_none()
    );
}

/// A signed kick is shown as a kick by its author, and the kicked
/// member's roster row goes.
#[tokio::test]
async fn a_verified_kick_names_its_moderator() {
    let w = World::new().await;
    w.by_owner(vec![w.add(&w.other)]).await;
    let (x_log, _, _) = crate::services::channel::membership_log::test_support::device(
        &w.other,
        &w.server,
        &[&w.owner, &w.me, &w.other],
    );
    w.author(&x_log, vec![act(Action::AcceptInvite(pb::AcceptInvite {}))])
        .await;
    w.by_owner(vec![act(Action::RemoveMember(pb::RemoveMember {
        user_id: w.other.id.clone(),
        reason: RmReason::Kicked as i32,
    }))])
    .await;
    let mut rx = w.bus.domain_sender().subscribe();
    w.relay_last().await; // gap: catches up the accept, then the kick
    let notices = w
        .service
        .message_repo
        .list(&w.ch, &ConversationType::Channel, None, 20)
        .unwrap();
    assert!(notices.iter().any(|m| matches!(&m.content,
        MessageContent::System(SystemMessage::UserRemoved { by, removed, .. })
            if *by == World::uid(&w.owner) && *removed == World::uid(&w.other))));
    let mut left = false;
    while let Ok(ev) = rx.try_recv() {
        left |= matches!(ev, DomainEvent::ChannelMemberLeft { .. });
    }
    assert!(left);
    assert!(
        !w.my_log
            .load(&w.ch)
            .unwrap()
            .unwrap()
            .state
            .members
            .contains_key(&w.other.id)
    );
}

/// The server's roster: owner and us, as `GetChannelMembers` lists them.
fn server_roster(w: &World) -> Vec<Membership> {
    [&w.owner, &w.me]
        .into_iter()
        .map(|p| Membership {
            channel_id: w.ch,
            user_id: World::uid(p),
            role: crate::domain::channel::MemberRole::Member,
            notification_preference: crate::domain::channel::NotificationPreference::All,
            joined_at: Utc::now() - chrono::Duration::days(1),
            last_read_message_id: None,
            is_muted: false,
            muted_until: None,
        })
        .collect()
}

fn removal(w: &Who, reason: RmReason) -> pb::ChannelAction {
    act(Action::RemoveMember(pb::RemoveMember {
        user_id: w.id.clone(),
        reason: reason as i32,
    }))
}

/// F79: a kick, a leave and a ban that all happened while we were offline
/// read with their signed reasons once a roster sync runs — "Owner
/// removed Other", "Other left", "Owner banned Other" — never as an
/// unexplained departure: the sync catches the signed log up before it
/// diffs the roster.
#[tokio::test]
async fn offline_kick_leave_and_ban_read_with_their_signed_reasons() {
    let w = World::new().await;
    let people = [&w.owner, &w.me, &w.other];
    let (x_log, _, _) = crate::services::channel::membership_log::test_support::device(
        &w.other, &w.server, &people,
    );
    let accept = || vec![act(Action::AcceptInvite(pb::AcceptInvite {}))];
    w.by_owner(vec![w.add(&w.other)]).await;
    w.author(&x_log, accept()).await;
    w.relay_last().await;
    assert!(
        w.channel_repo
            .get_membership(&w.ch, &World::uid(&w.other))
            .unwrap()
            .is_some()
    );

    // Offline from here: nothing is relayed to us.
    w.by_owner(vec![removal(&w.other, RmReason::Kicked)]).await;
    w.by_owner(vec![w.add(&w.other)]).await;
    x_log.catch_up(&w.ch).await.unwrap();
    w.author(&x_log, accept()).await;
    w.author(&x_log, vec![removal(&w.other, RmReason::Left)])
        .await;
    w.by_owner(vec![w.add(&w.other)]).await;
    x_log.catch_up(&w.ch).await.unwrap();
    w.author(&x_log, accept()).await;
    w.by_owner(vec![removal(&w.other, RmReason::Banned)]).await;

    // Back online: the roster sync for the channel.
    w.service
        .reconcile_roster(
            &w.ch,
            crate::domain::channel::ChannelType::Private,
            &server_roster(&w),
            None,
        )
        .await
        .unwrap();

    let (owner, other) = (World::uid(&w.owner), World::uid(&w.other));
    let notices: Vec<SystemMessage> = w
        .service
        .message_repo
        .list(&w.ch, &ConversationType::Channel, None, 50)
        .unwrap()
        .into_iter()
        .filter_map(|m| match m.content {
            MessageContent::System(sm) => Some(sm),
            _ => None,
        })
        .collect();
    let has = |want: &dyn Fn(&SystemMessage) -> bool| notices.iter().filter(|n| want(n)).count();
    assert_eq!(
        has(
            &|n| matches!(n, SystemMessage::UserRemoved { by, removed, .. } if *by == owner && *removed == other)
        ),
        1,
        "{notices:?}"
    );
    assert_eq!(
        has(&|n| matches!(n, SystemMessage::UserLeft(u) if *u == other)),
        1,
        "{notices:?}"
    );
    assert_eq!(
        has(
            &|n| matches!(n, SystemMessage::UserBanned { by, banned, .. } if *by == owner && *banned == other)
        ),
        1,
        "{notices:?}"
    );
    assert_eq!(
        has(&|n| matches!(n, SystemMessage::UserGone(_))),
        0,
        "an explained removal read as unexplained: {notices:?}"
    );
    assert!(
        w.channel_repo
            .get_membership(&w.ch, &other)
            .unwrap()
            .is_none()
    );
}

/// `GetDeparture` for us after `change` (revision N) removed us: the
/// change and the server's state at N-1.
fn departure_for(w: &World, change: pb::ChannelChange) -> pb::Departure {
    let rev = <pb::channel_change::Actions as prost::Message>::decode(change.actions.as_slice())
        .unwrap()
        .revision;
    let prev = w.server.states.lock()[rev as usize - 2].to_proto();
    pb::Departure {
        reason: pb::departure::Reason::Removed as i32,
        change: Some(change),
        prev_state: Some(prev),
    }
}

fn own_row(w: &World) -> Membership {
    w.channel_repo
        .get_membership(&w.ch, &World::uid(&w.me))
        .unwrap()
        .expect("our own roster row")
}

/// F79: kicked while offline, the channel gone from our list: the sync
/// shows "Owner removed you" from the signed kick, keeps the channel
/// read-only, and hearing the same kick live later files nothing more.
#[tokio::test]
async fn offline_self_kick_is_shown_from_the_signed_change() {
    let w = World::new().await;
    let own = own_row(&w);
    w.by_owner(vec![removal(&w.me, RmReason::Kicked)]).await;
    let kick = w.last();

    w.service
        .record_departure(&own, &departure_for(&w, kick.clone()))
        .await
        .unwrap();
    let base = w.count();
    let newest = w
        .service
        .message_repo
        .list(&w.ch, &ConversationType::Channel, None, 1)
        .unwrap();
    assert_eq!(
        newest[0].content,
        MessageContent::System(SystemMessage::SelfRemoved {
            by: World::uid(&w.owner)
        })
    );
    assert!(
        w.channel_repo
            .get_membership(&w.ch, &World::uid(&w.me))
            .unwrap()
            .is_none()
    );
    assert_eq!(
        w.channel_repo.former_state(&w.ch).unwrap(),
        Some(false),
        "listed read-only, history kept"
    );

    w.relay(kick).await; // a public channel's log is still readable
    assert_eq!(w.count(), base, "the same kick filed twice");
}

/// F79: banned after changes we never saw (our verified state is behind):
/// the ban verifies against the N-1 state its author's signature commits
/// to, and reads "Owner banned you". A leave on another of our devices
/// reads "You left".
#[tokio::test]
async fn offline_self_ban_behind_our_state_and_own_leave() {
    let w = World::new().await;
    let own = own_row(&w);
    w.by_owner(vec![w.add(&w.other)]).await; // unseen
    w.by_owner(vec![removal(&w.me, RmReason::Banned)]).await;
    w.service
        .record_departure(&own, &departure_for(&w, w.last()))
        .await
        .unwrap();
    let newest = w
        .service
        .message_repo
        .list(&w.ch, &ConversationType::Channel, None, 1)
        .unwrap();
    assert_eq!(
        newest[0].content,
        MessageContent::System(SystemMessage::SelfBanned {
            by: World::uid(&w.owner)
        })
    );

    let w = World::new().await;
    let own = own_row(&w);
    let (other_device, _, _) = crate::services::channel::membership_log::test_support::device(
        &w.me,
        &w.server,
        &[&w.owner, &w.me, &w.other],
    );
    w.author(&other_device, vec![removal(&w.me, RmReason::Left)])
        .await;
    w.service
        .record_departure(&own, &departure_for(&w, w.last()))
        .await
        .unwrap();
    let newest = w
        .service
        .message_repo
        .list(&w.ch, &ConversationType::Channel, None, 1)
        .unwrap();
    assert_eq!(
        newest[0].content,
        MessageContent::System(SystemMessage::UserLeft(World::uid(&w.me)))
    );
}

/// F79: the server's departure is believed only as far as the signature
/// goes. A doctored N-1 state, a removal of someone else, and a replay of
/// a removal older than our state all verify to nothing, and the notice
/// states no reason.
#[tokio::test]
async fn an_unverifiable_departure_states_no_reason() {
    let w = World::new().await;
    w.by_owner(vec![w.add(&w.other)]).await; // our state is now behind
    w.by_owner(vec![removal(&w.me, RmReason::Kicked)]).await;
    let mut doctored = departure_for(&w, w.last());
    doctored.prev_state.as_mut().unwrap().disappearing_seconds = 60;
    assert_eq!(
        removal_of_in(&w, &doctored).await,
        None,
        "a doctored base verified"
    );
    assert!(
        removal_of_in(&w, &departure_for(&w, w.last()))
            .await
            .is_some()
    );

    // Someone else's removal handed over as ours.
    let w = World::new().await;
    let x = crate::services::channel::membership_log::test_support::device(
        &w.other,
        &w.server,
        &[&w.owner, &w.me, &w.other],
    )
    .0;
    w.by_owner(vec![w.add(&w.other)]).await;
    w.author(&x, vec![act(Action::AcceptInvite(pb::AcceptInvite {}))])
        .await;
    w.by_owner(vec![removal(&w.other, RmReason::Kicked)]).await;
    let theirs = departure_for(&w, w.last());
    assert_eq!(
        removal_of_in(&w, &theirs).await,
        None,
        "another member's kick read as ours"
    );

    // An old kick replayed after we were re-added and caught up.
    let w = World::new().await;
    w.by_owner(vec![removal(&w.me, RmReason::Kicked)]).await;
    let old_kick = departure_for(&w, w.last());
    w.by_owner(vec![w.add(&w.me)]).await;
    w.my_log.catch_up(&w.ch).await.unwrap();
    w.author(
        &w.my_log,
        vec![act(Action::AcceptInvite(pb::AcceptInvite {}))],
    )
    .await;
    assert_eq!(
        removal_of_in(&w, &old_kick).await,
        None,
        "a replayed old kick verified"
    );
    let own = own_row(&w);
    w.service.record_departure(&own, &old_kick).await.unwrap();
    let newest = w
        .service
        .message_repo
        .list(&w.ch, &ConversationType::Channel, None, 1)
        .unwrap();
    assert_eq!(
        newest[0].content,
        MessageContent::System(SystemMessage::NoLongerMember)
    );
}

async fn removal_of_in(
    w: &World,
    d: &pb::Departure,
) -> Option<crate::services::channel::membership_log::OwnRemoval> {
    w.my_log
        .verify_own_removal(&w.ch, d.change.as_ref().unwrap(), d.prev_state.as_ref())
        .await
        .unwrap()
}

/// The old unsigned path is gone: a roster sync naming a user nobody in
/// the channel signed in shows them, but they never enter the verified
/// distribution set, and the user is told.
#[tokio::test]
async fn an_unsigned_roster_add_is_ignored_for_keys_and_surfaced() {
    let w = World::new().await;
    let mut rx = w.bus.domain_sender().subscribe();
    let ghost = UserId::from(uuid::Uuid::new_v4());
    let row = Membership {
        channel_id: w.ch,
        user_id: ghost,
        role: crate::domain::channel::MemberRole::Admin,
        notification_preference: crate::domain::channel::NotificationPreference::All,
        joined_at: Utc::now(),
        last_read_message_id: None,
        is_muted: false,
        muted_until: None,
    };
    w.service.record_synced_join(&row);
    let v = w.my_log.load(&w.ch).unwrap().unwrap();
    use crate::services::channel::verified_state::VerifiedMembership;
    assert!(!v.contains(&ghost.to_string()));
    let mut surfaced = false;
    while let Ok(ev) = rx.try_recv() {
        surfaced |= matches!(
            ev,
            DomainEvent::ChannelMembershipNotice {
                notice: MembershipNotice::UnverifiedMember { .. },
                ..
            }
        );
    }
    assert!(surfaced, "the unsigned add was not surfaced");
}

/// Our own leave files "left the channel" even though the leave already
/// deleted our membership row.
#[tokio::test]
async fn own_leave_files_a_notice_without_a_membership_row() {
    let (service, bus, channel_repo) = setup();
    let mut rx = bus.domain_sender().subscribe();
    let me = UserId::from(uuid::Uuid::new_v4());
    service
        .settings_repo
        .set_string(
            crate::storage::repositories::settings_repo::keys::CURRENT_USER_ID,
            &me.to_string(),
        )
        .unwrap();
    let channel = crate::domain::channel::Channel::new(
        ChannelId::from(uuid::Uuid::new_v4()),
        "invtest".to_string(),
        crate::domain::channel::ChannelType::Private,
        UserId::from(uuid::Uuid::new_v4()),
    );
    channel_repo.upsert(&channel).unwrap();

    service.record_own_leave(&channel.id);

    // Filed where the channel's history and sidebar preview read.
    assert_eq!(
        service
            .message_repo
            .count(&channel.id, &ConversationType::Channel)
            .unwrap(),
        1
    );
    match rx.try_recv() {
        Ok(DomainEvent::ChannelMemberLeft {
            user_id, message, ..
        }) => {
            assert_eq!(user_id, me);
            assert_eq!(message.sender_id, Some(me));
            assert!(
                matches!(message.content, MessageContent::System(SystemMessage::UserLeft(u)) if u == me)
            );
        }
        other => panic!("expected ChannelMemberLeft, got {other:?}"),
    }
}

fn as_me(service: &ChannelEventService) -> UserId {
    let me = UserId::from(uuid::Uuid::new_v4());
    service
        .settings_repo
        .set_string(
            crate::storage::repositories::settings_repo::keys::CURRENT_USER_ID,
            &me.to_string(),
        )
        .unwrap();
    me
}

fn private_channel(channel_repo: &ChannelRepository) -> ChannelId {
    let channel = crate::domain::channel::Channel::new(
        ChannelId::from(uuid::Uuid::new_v4()),
        "invtest".to_string(),
        crate::domain::channel::ChannelType::Private,
        UserId::from(uuid::Uuid::new_v4()),
    );
    channel_repo.upsert(&channel).unwrap();
    channel.id
}

/// F79: our own leave arrives as our own committed change and from the
/// leave RPC's `record_own_leave`, in either order. One "You left" and
/// one UI event.
#[tokio::test]
async fn own_leave_seen_on_both_paths_files_one_notice() {
    let w = World::new().await;
    let mut rx = w.bus.domain_sender().subscribe();
    let base = w.count();
    w.author(
        &w.my_log,
        vec![act(Action::RemoveMember(pb::RemoveMember {
            user_id: w.me.id.clone(),
            reason: RmReason::Left as i32,
        }))],
    )
    .await;
    w.relay_last().await; // the server's echo: a duplicate
    w.service.record_own_leave(&w.ch);
    assert_eq!(w.count(), base + 1, "our leave filed two notices");
    assert!(matches!(
        rx.try_recv(),
        Ok(DomainEvent::ChannelMemberLeft { .. })
    ));
    assert!(rx.try_recv().is_err(), "the duplicate reached the UI");
    assert_eq!(w.channel_repo.former_state(&w.ch).unwrap(), Some(false));
}

/// F80: our own Leave (the RPC path: `leave_channel` deletes our
/// membership, then `record_own_leave`) keeps the channel listed
/// read-only — former, not deleted — with our "left the channel" notice.
/// RC20 saw a left channel vanish, but that channel was left before F80.
#[tokio::test]
async fn own_leave_keeps_the_channel_listed_read_only() {
    let (service, _bus, channel_repo) = setup();
    let me = as_me(&service);
    let channel_id = private_channel(&channel_repo);
    let own = Membership {
        channel_id,
        user_id: me,
        role: crate::domain::channel::MemberRole::Member,
        notification_preference: crate::domain::channel::NotificationPreference::All,
        joined_at: Utc::now(),
        last_read_message_id: None,
        is_muted: false,
        muted_until: None,
    };
    channel_repo.insert_or_ignore_membership(&own).unwrap();

    channel_repo.delete_membership(&channel_id, &me).unwrap();
    service.record_own_leave(&channel_id);

    assert_eq!(channel_repo.former_state(&channel_id).unwrap(), Some(false));
    let listed = channel_repo.list_former_for_user(&me).unwrap();
    assert_eq!(
        listed.iter().map(|c| c.id).collect::<Vec<_>>(),
        [channel_id],
        "the left channel vanished"
    );
    let notices = service
        .message_repo
        .list(&channel_id, &ConversationType::Channel, None, 10)
        .unwrap();
    assert_eq!(notices.len(), 1);
    assert_eq!(
        notices[0].content,
        MessageContent::System(SystemMessage::UserLeft(me))
    );
    assert_eq!(
        crate::domain::channel::SendBlock::for_viewer(None, false, Some(false)),
        Some(crate::domain::channel::SendBlock::Removed),
        "a left channel must replace the composer, not accept sends"
    );
}

/// F79: a roster sync that finds a member gone (or new) files the notice
/// the missed live event would have, once however many syncs see it.
#[tokio::test]
async fn synced_roster_changes_file_one_notice_each() {
    let (service, bus, channel_repo) = setup();
    let mut rx = bus.domain_sender().subscribe();
    let _me = as_me(&service);
    let channel_id = private_channel(&channel_repo);
    let count = || {
        service
            .message_repo
            .count(&channel_id, &ConversationType::Channel)
            .unwrap()
    };
    let member = Membership {
        channel_id,
        user_id: UserId::from(uuid::Uuid::new_v4()),
        role: crate::domain::channel::MemberRole::Member,
        notification_preference: crate::domain::channel::NotificationPreference::All,
        joined_at: Utc::now(),
        last_read_message_id: None,
        is_muted: false,
        muted_until: None,
    };

    service.record_synced_join(&member);
    service.record_synced_join(&member);
    assert_eq!(count(), 1);
    assert!(matches!(
        rx.try_recv(),
        Ok(DomainEvent::ChannelMemberJoined { .. })
    ));
    assert!(rx.try_recv().is_err());

    service.record_synced_departure(&member);
    service.record_synced_departure(&member);
    assert_eq!(count(), 2);
    match rx.try_recv() {
        Ok(DomainEvent::ChannelMemberLeft {
            user_id, message, ..
        }) => {
            assert_eq!(user_id, member.user_id);
            // No signed change says why: never worded as a leave (F79).
            assert!(
                matches!(message.content, MessageContent::System(SystemMessage::UserGone(u)) if u == member.user_id)
            );
        }
        other => panic!("expected ChannelMemberLeft, got {other:?}"),
    }
    assert!(rx.try_recv().is_err());
    assert!(
        channel_repo
            .get_membership(&channel_id, &member.user_id)
            .unwrap()
            .is_none()
    );
}

/// F79: our own removal found by a sync with no signed change to show
/// (or none that verifies) reads "You are no longer a member" — not the
/// server's word for why — and the channel stays listed read-only. A
/// deletion reads as one; "still a member" changes nothing.
#[tokio::test]
async fn synced_own_departure_without_a_signed_change_states_no_reason() {
    use crate::proto::chat_v1::{Departure, departure::Reason as Dep};
    let (service, _bus, channel_repo) = setup();
    let me = as_me(&service);
    let own_in = |channel_id| {
        let own = Membership {
            channel_id,
            user_id: me,
            role: crate::domain::channel::MemberRole::Member,
            notification_preference: crate::domain::channel::NotificationPreference::All,
            joined_at: Utc::now(),
            last_read_message_id: None,
            is_muted: false,
            muted_until: None,
        };
        channel_repo.insert_or_ignore_membership(&own).unwrap();
        own
    };

    let removed = own_in(private_channel(&channel_repo));
    let departure = Departure {
        reason: Dep::Removed as i32,
        ..Default::default()
    };
    service
        .record_departure(&removed, &departure)
        .await
        .unwrap();
    service
        .record_departure(&removed, &departure)
        .await
        .unwrap();
    assert!(
        channel_repo
            .get_membership(&removed.channel_id, &me)
            .unwrap()
            .is_none()
    );
    assert_eq!(
        channel_repo.former_state(&removed.channel_id).unwrap(),
        Some(false)
    );
    let notices = service
        .message_repo
        .list(&removed.channel_id, &ConversationType::Channel, None, 10)
        .unwrap();
    assert_eq!(notices.len(), 1, "filed once however many syncs see it");
    assert_eq!(
        notices[0].content,
        MessageContent::System(SystemMessage::NoLongerMember)
    );

    let deleted = own_in(private_channel(&channel_repo));
    service
        .record_departure(
            &deleted,
            &Departure {
                reason: Dep::ChannelDeleted as i32,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(
        channel_repo.former_state(&deleted.channel_id).unwrap(),
        Some(true)
    );

    let still = own_in(private_channel(&channel_repo));
    service
        .record_departure(
            &still,
            &Departure {
                reason: Dep::Member as i32,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(
        channel_repo
            .get_membership(&still.channel_id, &me)
            .unwrap()
            .is_some()
    );
}

/// Mute is an unsigned server action (routing only): it is mirrored onto
/// the roster row and reaches the UI; the member stays a member.
#[tokio::test]
async fn moderation_mute_is_mirrored_and_announced() {
    let (service, bus, channel_repo) = setup();
    let mut rx = bus.domain_sender().subscribe();
    let me = as_me(&service);
    let channel_id = private_channel(&channel_repo);
    channel_repo
        .insert_membership(&Membership {
            channel_id,
            user_id: me,
            role: crate::domain::channel::MemberRole::Member,
            notification_preference: crate::domain::channel::NotificationPreference::All,
            joined_at: Utc::now(),
            last_read_message_id: None,
            is_muted: false,
            muted_until: None,
        })
        .unwrap();

    service
        .handle_stream_event(
            channel_event::Event::ModerationAction(ChannelModeration {
                channel_id: channel_id.to_string(),
                user_id: me.to_string(),
                moderator_id: uuid::Uuid::new_v4().to_string(),
                action: crate::proto::chat_v1::channel_moderation::Action::Mute as i32,
                duration: Some(prost_types::Duration {
                    seconds: 600,
                    nanos: 0,
                }),
                reason: "spam".to_string(),
            }),
            &channel_id.to_string(),
        )
        .await;

    let row = channel_repo
        .get_membership(&channel_id, &me)
        .unwrap()
        .expect("still a member");
    assert!(row.is_muted);
    assert!(row.muted_until.is_some());
    let mut moderation = None;
    while let Ok(ev) = rx.try_recv() {
        if let DomainEvent::ChannelModerationAction {
            action,
            is_self,
            reason,
            ..
        } = ev
        {
            moderation = Some((action, is_self, reason));
        }
    }
    let (action, is_self, reason) =
        moderation.expect("ModerationAction did not reach the domain bus");
    assert_eq!(action, "MUTE");
    assert!(is_self);
    assert_eq!(reason.as_deref(), Some("spam"));
}

// Connected regressions use the production recorder and production delete builder.
use super::*;
use crate::domain::anon_dm::{AnonDm, AnonDmStatus};
use crate::domain::call::{CallDirection, CallHistoryEntry, CallResult};
use crate::domain::ids::MessageId;
use crate::events::DomainEvent;
use crate::proto::content_v1::{self, Content};
use crate::services::call_history_recorder::{dm_call_bubble, file_dm_bubble};
use crate::services::gateway::builders::build_stored_dm_delete_content;
use crate::storage::repositories::CallHistoryRepository;

fn entry(
    id: uuid::Uuid,
    conversation: ChannelId,
    kind: ConversationType,
    direction: CallDirection,
    result: CallResult,
) -> CallHistoryEntry {
    CallHistoryEntry {
        id: if kind == ConversationType::AnonDm {
            format!("anon-{id}")
        } else {
            id.to_string()
        },
        conversation_id: conversation.to_string(),
        conversation_type: kind,
        peer_name: "actual peer".into(),
        started_at: chrono::Utc::now(),
        ended_at: Some(chrono::Utc::now()),
        direction,
        result,
        duration_secs: Some(3),
        has_video: false,
    }
}

fn decoded(content: Content) -> Content {
    <Content as prost::Message>::decode(prost::Message::encode_to_vec(&content).as_slice()).unwrap()
}
fn delete_mut(content: &mut Content) -> &mut content_v1::Delete {
    let Some(content_v1::content::Content::DataMessage(data)) = &mut content.content else {
        panic!("data")
    };
    data.delete.as_mut().unwrap()
}

async fn normal_receive(f: &Fixture, sender: UserId, content: Content) {
    normal_receive_from_mailbox(f, sender, sender, content).await;
}
async fn normal_receive_from_mailbox(
    f: &Fixture,
    sender: UserId,
    mailbox_peer: UserId,
    content: Content,
) {
    use crate::inbox::{polling::device::dispatch_control_content, state::InboxStats};
    use crate::services::inbox_processor::InboxEventProcessor;
    let (tx, mut rx) = tokio::sync::broadcast::channel(8);
    let stats = Arc::new(parking_lot::RwLock::new(InboxStats::default()));
    let content = decoded(content);
    let svc = f.client.messages();
    let me = svc.current_user_id().unwrap();
    if sender != me {
        if let Ok(parsed) = crate::services::gateway::MessageGateway::parse_content(content.clone())
        {
            let manager = crate::mailbox::dm::DmConversationManager::new(svc.database.clone());
            crate::inbox::polling::dm::dispatch_dm_content(
                parsed,
                &sender,
                1,
                mailbox_peer,
                "actual call",
                3,
                &Some(me),
                &tx,
                &svc.message_repo,
                &svc.attachment_repo,
                &svc.user_repo,
                &manager,
                None,
            );
        } else {
            dispatch_control_content(
                content.content.unwrap(),
                &sender,
                1,
                "actual call",
                3,
                &tx,
                &stats,
            );
        }
    } else {
        dispatch_control_content(
            content.content.unwrap(),
            &sender,
            1,
            "actual call",
            3,
            &tx,
            &stats,
        );
    }
    if let Ok(event) = rx.try_recv() {
        InboxEventProcessor::from_client(&f.client)
            .process(&event)
            .await;
    }
}

#[tokio::test]
async fn stable_call_actual_replicas_normal_and_anonymous_both_directions() {
    let a = fixture();
    let b = fixture();
    b.client
        .messages()
        .settings_repo
        .set(keys::CURRENT_USER_ID, &a.carol.to_string())
        .unwrap();
    a.client
        .auth_state()
        .set_logged_in(a.me, 1, "call fixture".into());
    b.client
        .auth_state()
        .set_logged_in(a.carol, 1, "call fixture".into());
    for kind in [ConversationType::ServerDm, ConversationType::AnonDm] {
        let anon_id = crate::domain::anon_dm::AnonDmId::new();
        if kind == ConversationType::AnonDm {
            for (f, peer) in [(&a, a.carol), (&b, a.me)] {
                let mut dm = AnonDm::new_initiated(peer);
                dm.id = anon_id;
                dm.status = AnonDmStatus::Active;
                dm.peer_mailbox_pk = Some([7; 32]);
                f.client
                    .anon_dms()
                    .anon_dm_repo
                    .insert_conversation(&dm)
                    .unwrap();
            }
        }
        for result in [
            CallResult::Completed,
            CallResult::Declined,
            CallResult::Missed,
        ] {
            for outgoing in [true, false] {
                for a_deletes in [true, false] {
                    let id = uuid::Uuid::new_v4();
                    let direction = if outgoing {
                        CallDirection::Outgoing
                    } else {
                        CallDirection::Incoming
                    };
                    let opposite = if outgoing {
                        CallDirection::Incoming
                    } else {
                        CallDirection::Outgoing
                    };
                    let mut a_row = entry(
                        id,
                        if kind == ConversationType::ServerDm {
                            a.carol.into()
                        } else {
                            anon_id.into()
                        },
                        kind.clone(),
                        direction,
                        result.clone(),
                    );
                    let mut b_row = entry(
                        id,
                        if kind == ConversationType::ServerDm {
                            a.me.into()
                        } else {
                            anon_id.into()
                        },
                        kind.clone(),
                        opposite,
                        result.clone(),
                    );
                    if kind == ConversationType::AnonDm && !a_deletes {
                        // The actual constructor also accepts historical raw UUID handles.
                        a_row.id = id.to_string();
                        b_row.id = id.to_string();
                    }
                    let first = dm_call_bubble(&a_row, a.me).unwrap();
                    let second = dm_call_bubble(&b_row, a.carol).unwrap();
                    assert_eq!(first.id, second.id);
                    assert_ne!(
                        first.sent_at, second.sent_at,
                        "real recorder stamps are local"
                    );
                    if kind == ConversationType::ServerDm {
                        assert_ne!(first.sender_id, second.sender_id);
                    } else {
                        assert_eq!(first.sender_id, Some(UserId(uuid::Uuid::nil())));
                    }
                    a.client
                        .messages()
                        .store_message_local_once(&first)
                        .unwrap();
                    b.client
                        .messages()
                        .store_message_local_once(&second)
                        .unwrap();
                    // Local menu selects a real stored production Call (including nil AnonDm).
                    for (f, record, peer) in [(&a, &first, a.carol), (&b, &second, a.me)] {
                        let active = if kind == ConversationType::ServerDm {
                            crate::state::ActiveConversation::ServerDm { peer_id: peer }
                        } else {
                            crate::state::ActiveConversation::AnonDm(anon_id)
                        };
                        assert!(
                            f.client
                                .conversation(active)
                                .can_delete_for_everyone(&record.sender_id.unwrap(), record.sent_at)
                                .unwrap()
                        );
                    }
                    let pair = if a_deletes {
                        (&a, &b, &first, &second, a.me)
                    } else {
                        (&b, &a, &second, &first, a.carol)
                    };
                    for (sender, receiver, record, receiver_record, sender_id) in [pair] {
                        assert!(
                            sender
                                .client
                                .messages()
                                .message_repo
                                .get(&record.id)
                                .unwrap()
                                .is_some()
                        );
                        let active = if kind == ConversationType::ServerDm {
                            crate::state::ActiveConversation::ServerDm {
                                peer_id: if a_deletes { a.carol } else { a.me },
                            }
                        } else {
                            crate::state::ActiveConversation::AnonDm(anon_id)
                        };
                        let (selected, wire) = sender
                            .client
                            .conversation(active)
                            .prepare_delete_for_everyone(&record.sender_id.unwrap(), record.sent_at)
                            .unwrap();
                        assert_eq!(selected.id, record.id);
                        assert_eq!(
                            delete_mut(&mut wire.clone()).target_call_id.as_deref(),
                            Some(id.to_string().as_str())
                        );
                        if kind == ConversationType::ServerDm {
                            let mut events = receiver.client.event_bus().subscribe_domain();
                            normal_receive(receiver, sender_id, wire).await;
                            assert!(
                                matches!(events.try_recv().unwrap(), DomainEvent::MessageDeleted { message_id, target_sent_timestamp, .. }
                            if message_id == receiver_record.id && target_sent_timestamp == receiver_record.sent_at)
                            );
                        } else {
                            let events = receiver
                                .client
                                .anon_dms()
                                .process_incoming_content(&anon_id, &sender_id, &decoded(wire))
                                .unwrap();
                            assert!(
                                matches!(events.as_slice(), [DomainEvent::MessageDeleted { message_id, target_sent_timestamp, .. }]
                            if *message_id == receiver_record.id && *target_sent_timestamp == receiver_record.sent_at)
                            );
                        }
                        assert!(
                            receiver
                                .client
                                .messages()
                                .message_repo
                                .get(&receiver_record.id)
                                .unwrap()
                                .is_none()
                        );
                        assert!(
                            !receiver
                                .client
                                .messages()
                                .store_message_local_once(receiver_record)
                                .unwrap(),
                            "replay suppressed"
                        );
                    }
                }
            }
        }
    }
}

#[tokio::test]
async fn stable_call_late_actual_recorder_and_linked_device_no_ghost_or_history_loss() {
    let f = fixture();
    for kind in [ConversationType::ServerDm, ConversationType::AnonDm] {
        let id = uuid::Uuid::new_v4();
        let mut dm = AnonDm::new_initiated(f.carol);
        dm.status = AnonDmStatus::Active;
        dm.peer_mailbox_pk = Some([7; 32]);
        f.client
            .anon_dms()
            .anon_dm_repo
            .insert_conversation(&dm)
            .unwrap();
        let row = entry(
            id,
            if kind == ConversationType::ServerDm {
                f.carol.into()
            } else {
                dm.id.into()
            },
            kind.clone(),
            CallDirection::Incoming,
            CallResult::Missed,
        );
        let peer_row = entry(
            id,
            if kind == ConversationType::ServerDm {
                f.me.into()
            } else {
                dm.id.into()
            },
            kind.clone(),
            CallDirection::Outgoing,
            CallResult::Missed,
        );
        let peer_bubble = dm_call_bubble(&peer_row, f.carol).unwrap();
        let wire = build_stored_dm_delete_content(&peer_bubble, &f.carol).unwrap();
        if kind == ConversationType::ServerDm {
            // Actual own-device encoder supplies field 102, preserving 101/103.
            let copy = super::super::dm::own_device_copy(&f.carol, &wire, 1).unwrap();
            normal_receive(&f, f.me, copy).await;
        } else {
            assert!(
                f.client
                    .anon_dms()
                    .process_incoming_content(&dm.id, &f.carol, &decoded(wire))
                    .unwrap()
                    .is_empty()
            );
        }
        let history = CallHistoryRepository::new(f.client.messages().database.clone());
        history.insert_entry(&row).unwrap();
        let handle = crate::services::call_worker::CallHandle::from(row.id.clone());
        let mut events = f.client.event_bus().subscribe_domain();
        file_dm_bubble(&history, f.client.messages(), &handle);
        file_dm_bubble(&history, f.client.messages(), &handle);
        assert!(
            events.try_recv().is_err(),
            "late actual file path must emit no ghost MessageReceived"
        );
        assert!(
            f.client
                .messages()
                .message_repo
                .get(&MessageId::from_uuid(id))
                .unwrap()
                .is_none()
        );
        assert!(
            history.get(&row.id).unwrap().is_some(),
            "Calls screen history stays local and intact"
        );
        // Timestamp-zero ordinary message with the tombstone's synthetic author is never matched.
        let ordinary = Message::new_text(
            row.conversation_id.parse().unwrap(),
            kind.clone(),
            Some(UserId(id)),
            "zero text".into(),
            0,
        );
        assert!(
            f.client
                .messages()
                .store_message_local_once(&ordinary)
                .unwrap()
        );
        // Same UUID in another type remains insertable: no global ID tombstone.
        let mut other_type = dm_call_bubble(&row, f.me).unwrap();
        other_type.conversation_type = ConversationType::Channel;
        assert!(
            f.client
                .messages()
                .store_message_local_once(&other_type)
                .unwrap()
        );
    }
}

#[tokio::test]
async fn stable_call_forged_noncall_malformed_scope_and_destination_refused() {
    let f = fixture();
    let svc = f.client.messages();
    let row = entry(
        uuid::Uuid::new_v4(),
        f.carol.into(),
        ConversationType::ServerDm,
        CallDirection::Incoming,
        CallResult::Completed,
    );
    let bubble = dm_call_bubble(&row, f.me).unwrap();
    svc.store_message_local_once(&bubble).unwrap();
    let wire = build_stored_dm_delete_content(&bubble, &f.me).unwrap();
    for claim in [
        "",
        "garbage",
        "00000000-0000-0000-0000-000000000000",
        "anon-00000000-0000-0000-0000-000000000000",
    ] {
        let mut forged = wire.clone();
        delete_mut(&mut forged).target_call_id = Some(claim.into());
        normal_receive(&f, f.carol, forged).await;
        assert!(svc.message_repo.get(&bubble.id).unwrap().is_some());
    }
    let mut wrong_scope = wire.clone();
    delete_mut(&mut wrong_scope).target_author_aci = Some(f.me.to_string());
    normal_receive_from_mailbox(&f, f.bob, f.carol, wrong_scope).await; // authenticated sender overrides mailbox route
    let mut forged = wire.clone();
    delete_mut(&mut forged).target_author_aci = Some(f.bob.to_string());
    normal_receive(&f, f.carol, forged).await;
    let mut forged = wire.clone();
    delete_mut(&mut forged).destination_user_id = Some(f.carol.to_string());
    normal_receive(&f, f.carol, forged).await;
    normal_receive(&f, f.me, wire.clone()).await; // self requires destination 102
    let mut forged = super::super::dm::own_device_copy(&f.carol, &wire, 1).unwrap();
    delete_mut(&mut forged).target_author_aci = None;
    normal_receive(&f, f.me, forged).await; // self requires author 101
    let mut forged = wire.clone();
    if let Some(content_v1::content::Content::DataMessage(data)) = &mut forged.content {
        data.channel_id = f.channel.to_string();
    }
    normal_receive(&f, f.carol, forged).await; // channel rejects call claim
    assert!(svc.message_repo.get(&bubble.id).unwrap().is_some());
    // Explicit call claims cannot delete a Text record even when UUID and timestamp match.
    let text = Message::new_text(
        f.carol.into(),
        ConversationType::ServerDm,
        Some(f.carol),
        "real text".into(),
        777,
    );
    svc.store_message_local_once(&text).unwrap();
    let mut forged = wire.clone();
    delete_mut(&mut forged).target_call_id = Some(text.id.to_string());
    delete_mut(&mut forged).target_sent_timestamp = text.sent_at;
    normal_receive(&f, f.carol, forged).await;
    assert!(svc.message_repo.get(&text.id).unwrap().is_some());
    // Inconsistent stored Call content/message-ID is refused by both sender and receiver.
    let mut bad = dm_call_bubble(&row, f.me).unwrap();
    bad.id = MessageId::new();
    svc.store_message_local_once(&bad).unwrap();
    assert!(build_stored_dm_delete_content(&bad, &f.me).is_err());
    let mut forged = wire.clone();
    delete_mut(&mut forged).target_call_id = Some(bad.id.to_string());
    normal_receive(&f, f.carol, forged).await;
    assert!(svc.message_repo.get(&bad.id).unwrap().is_some());
    let linked_record = dm_call_bubble(&row, f.me).unwrap();
    assert_ne!(linked_record.sent_at, bubble.sent_at);
    let linked_wire = build_stored_dm_delete_content(&linked_record, &f.me).unwrap();
    let copy = super::super::dm::own_device_copy(&f.carol, &linked_wire, 1).unwrap();
    normal_receive(&f, f.me, copy).await;
    assert!(svc.message_repo.get(&bubble.id).unwrap().is_none());
    assert!(svc.message_repo.get(&bad.id).unwrap().is_some());
}

#[tokio::test]
async fn stable_call_anonymous_forged_scope_noncall_and_group_claims_refused() {
    let f = fixture();
    let svc = f.client.messages();
    let anon = f.client.anon_dms();
    let mut dm = AnonDm::new_initiated(f.carol);
    dm.status = AnonDmStatus::Active;
    dm.peer_mailbox_pk = Some([7; 32]);
    anon.anon_dm_repo.insert_conversation(&dm).unwrap();
    let row = entry(
        uuid::Uuid::new_v4(),
        dm.id.into(),
        ConversationType::AnonDm,
        CallDirection::Incoming,
        CallResult::Declined,
    );
    let bubble = dm_call_bubble(&row, f.me).unwrap();
    svc.store_message_local_once(&bubble).unwrap();
    let wire = build_stored_dm_delete_content(&bubble, &f.me).unwrap();
    for claim in ["", "bad", "00000000-0000-0000-0000-000000000000"] {
        let mut bad = wire.clone();
        delete_mut(&mut bad).target_call_id = Some(claim.into());
        assert!(
            anon.process_incoming_content(&dm.id, &f.carol, &decoded(bad))
                .unwrap()
                .is_empty()
        );
    }
    assert!(
        anon.process_incoming_content(&dm.id, &f.bob, &wire)
            .unwrap()
            .is_empty()
    );
    assert!(
        anon.process_incoming_content(&crate::domain::anon_dm::AnonDmId::new(), &f.carol, &wire)
            .unwrap()
            .is_empty()
    );
    for author in [
        f.bob.to_string(),
        uuid::Uuid::nil().to_string(),
        "bad".into(),
    ] {
        let mut bad = wire.clone();
        delete_mut(&mut bad).target_author_aci = Some(author);
        assert!(
            anon.process_incoming_content(&dm.id, &f.carol, &bad)
                .unwrap()
                .is_empty()
        );
    }
    let mut bad = wire.clone();
    delete_mut(&mut bad).destination_user_id = Some(f.carol.to_string());
    assert!(
        anon.process_incoming_content(&dm.id, &f.carol, &bad)
            .unwrap()
            .is_empty()
    );
    let mut bad = wire.clone();
    if let Some(content_v1::content::Content::DataMessage(data)) = &mut bad.content {
        data.channel_id = dm.id.to_string();
    }
    assert!(
        anon.process_incoming_content(&dm.id, &f.carol, &bad)
            .unwrap()
            .is_empty()
    );
    assert!(svc.message_repo.get(&bubble.id).unwrap().is_some());
    let text = Message::new_text(
        dm.id.into(),
        ConversationType::AnonDm,
        Some(f.carol),
        "noncall".into(),
        444,
    );
    svc.store_message_local_once(&text).unwrap();
    let mut bad = wire.clone();
    delete_mut(&mut bad).target_call_id = Some(text.id.to_string());
    delete_mut(&mut bad).target_sent_timestamp = text.sent_at;
    assert!(
        anon.process_incoming_content(&dm.id, &f.carol, &bad)
            .unwrap()
            .is_empty()
    );
    assert!(svc.message_repo.get(&text.id).unwrap().is_some());
    // Author's own group message is still protected from any call-ID claim.
    let group = crate::domain::anon_group::AnonGroupId::new();
    let record = Message::new_text(
        group.into(),
        ConversationType::AnonGroup,
        Some(f.carol),
        "group survives".into(),
        555,
    );
    svc.store_message_local_once(&record).unwrap();
    let mut bad = crate::services::gateway::builders::build_targeted_delete_content(
        record.sent_at,
        Some(&f.carol.to_string()),
        None,
    );
    delete_mut(&mut bad).target_call_id = Some(bubble.id.to_string());
    let events = crate::inbox::handlers::base_anon::dispatch_common_content(
        &group,
        &f.carol,
        &bad,
        f.client.anon_groups().as_ref(),
    );
    assert!(matches!(
        events,
        crate::inbox::handlers::base_anon::CommonDispatchOutcome::Dropped
    ));
    assert!(
        !svc.message_repo
            .get(&record.id)
            .unwrap()
            .unwrap()
            .is_deleted
    );
    // Same UUID in wrong type cannot become a scoped delete target.
    let mut mismatched = dm_call_bubble(
        &entry(
            uuid::Uuid::new_v4(),
            dm.id.into(),
            ConversationType::AnonDm,
            CallDirection::Outgoing,
            CallResult::Completed,
        ),
        f.me,
    )
    .unwrap();
    mismatched.conversation_type = ConversationType::AnonGroup;
    svc.store_message_local_once(&mismatched).unwrap();
    let mut bad = wire.clone();
    delete_mut(&mut bad).target_call_id = Some(mismatched.id.to_string());
    assert!(
        anon.process_incoming_content(&dm.id, &f.carol, &bad)
            .unwrap()
            .is_empty()
    );
    assert!(svc.message_repo.get(&mismatched.id).unwrap().is_some());
}

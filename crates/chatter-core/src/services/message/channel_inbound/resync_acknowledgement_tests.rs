use super::*;

#[test]
fn resync_distinguishes_pending_application_from_rejected_content() {
    assert!(matches!(
        ensure_resync_ingested(Ingested::DeferredPendingSenderKey),
        Err(MessageError::ResyncPending)
    ));
    assert!(matches!(
        ensure_resync_ingested(Ingested::RetryableFailure),
        Err(MessageError::ResyncPending)
    ));
    assert!(matches!(
        ensure_resync_ingested(Ingested::Dropped),
        Err(MessageError::InvalidContent(_))
    ));
    assert!(ensure_resync_ingested(Ingested::Stored).is_ok());
}
#[tokio::test]
async fn peer_channel_reaction_survives_empty_or_partial_server_resync() {
    use crate::client::ChatterClient;
    use crate::domain::channel::{
        Channel, ChannelType, MemberRole, Membership, NotificationPreference,
    };
    use crate::domain::message::ConversationType;
    use crate::domain::reaction::ReactionScope;
    static MOCK: std::sync::Once = std::sync::Once::new();
    MOCK.call_once(|| keyring::set_default_store(crate::system::keyring::test_credential_store()));
    let dir = tempfile::tempdir().unwrap();
    let client = ChatterClient::builder()
        .server_url("https://localhost:1")
        .data_dir(dir.path().to_str().unwrap())
        .build()
        .unwrap();
    let own = UserId::new();
    client.auth_state().set_logged_in(own, 1, "fixture".into());
    let peer = UserId::new();
    let author = UserId::new();
    let id = ChannelId::new();
    let service = client.messages();
    service
        .settings_repo
        .set("auth.current_user_id", &own.to_string())
        .unwrap();
    service
        .settings_repo
        .set("auth.current_device_id", &1u32)
        .unwrap();
    for user in [own, peer] {
        service.user_repo.ensure_user_exists(&user).unwrap();
    }
    service
        .channel_repo
        .insert(&Channel::new(
            id,
            "resync reaction fixture".into(),
            ChannelType::Public,
            own,
        ))
        .unwrap();
    service
        .channel_repo
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
    let scope = ReactionScope::new(id, ConversationType::Channel);
    let message = crate::domain::message::Message::new_text(
        id,
        ConversationType::Channel,
        Some(author),
        "peer authority".into(),
        100,
    );
    service.store_message_local(&message).unwrap();
    assert!(
        service
            .record_inbound_reaction(&scope, &peer, &author, 100, "👍", false, 10)
            .unwrap()
    );
    assert!(
        service
            .record_inbound_reaction(&scope, &own, &author, 100, "👍", false, 11)
            .unwrap()
    );
    let inbound = ChannelInbound::new(
        service.clone(),
        client.attachments().clone(),
        client.users().clone(),
    );
    let mut envelope = chat_v1::Message {
        id: message.id.to_string(),
        channel_id: id.to_string(),
        author_id: author.to_string(),
        timestamp: 100,
        content: Some(chat_v1::message::Content {
            r#type: Some(chat_v1::message::content::Type::Text(
                "peer authority".into(),
            )),
        }),
        ..Default::default()
    };
    inbound.ingest_resynced(&envelope, id).await.unwrap();
    assert_eq!(
        service
            .message_repo
            .get_reaction_counts(&scope, &author, 100)
            .unwrap()[0]
            .count,
        2
    );
    assert!(
        service
            .user_has_reacted(&scope, &author, 100, "👍")
            .unwrap()
    );
    // A partial server projection has no authority to erase our peer vote.
    envelope.reactions.push(chat_v1::ReactionUpdate {
        target_author_aci: author.to_string(),
        target_sent_timestamp: 100,
        reactor_id: peer.to_string(),
        emoji: "👍".into(),
        ..Default::default()
    });
    inbound.ingest_resynced(&envelope, id).await.unwrap();
    assert_eq!(
        service
            .message_repo
            .get_reaction_counts(&scope, &author, 100)
            .unwrap()[0]
            .count,
        2
    );
    assert_eq!(inbound.ingest(&envelope, id, false).await, Ingested::Stored);
    assert!(
        service
            .user_has_reacted(&scope, &author, 100, "👍")
            .unwrap(),
        "live/backfill projection preserves own highlight too"
    );
}

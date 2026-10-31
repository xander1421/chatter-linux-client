use super::*;

#[tokio::test]
async fn resync_clock_conflict_keeps_local_history_and_allows_later_rows() {
    keyring::set_default_store(crate::system::keyring::test_credential_store());
    let dir = tempfile::tempdir().unwrap();
    let client = ChatterClient::builder()
        .server_url("https://localhost:1")
        .data_dir(dir.path().to_str().unwrap())
        .build()
        .unwrap();
    bind_test_owner(&client);
    let inbound = ChannelInbound::new(
        client.messages().clone(),
        client.attachments().clone(),
        client.users().clone(),
    );
    let channel = ChannelId::new();
    let id = crate::domain::ids::MessageId::new();
    let mut row = chat_v1::Message {
        id: id.to_string(),
        channel_id: channel.to_string(),
        author_id: UserId::new().to_string(),
        timestamp: 1_000,
        content: Some(chat_v1::message::Content {
            r#type: Some(chat_v1::message::content::Type::Text("retained".into())),
        }),
        ..Default::default()
    };
    assert_eq!(inbound.ingest(&row, channel, false).await, Ingested::Stored);
    let original = client.messages().get_message_local(&id).unwrap().unwrap();
    row.timestamp += 1;
    let rejected = inbound.ingest_resynced(&row, channel).await;
    assert!(
        matches!(&rejected, Err(MessageError::InvalidContent(reason))
        if reason == "resync message identity mismatch")
    );
    assert!(
        crate::services::channel_stream::resync_row_complete(
            rejected,
            &channel.to_string(),
            &row.id
        )
        .unwrap()
    );
    let retained = client.messages().get_message_local(&id).unwrap().unwrap();
    assert_eq!(retained.sent_at, original.sent_at);
    assert_eq!(retained.content, original.content);
    let next_id = crate::domain::ids::MessageId::new();
    row.id = next_id.to_string();
    assert!(
        crate::services::channel_stream::resync_row_complete(
            inbound.ingest_resynced(&row, channel).await,
            &channel.to_string(),
            &row.id
        )
        .unwrap()
    );
    assert!(
        client
            .messages()
            .get_message_local(&next_id)
            .unwrap()
            .is_some()
    );
    assert!(
        !crate::services::channel_stream::resync_row_complete(
            Err(MessageError::ResyncPending),
            &channel.to_string(),
            &row.id
        )
        .unwrap()
    );
}

use super::*;

#[tokio::test]
async fn failed_sqlite_write_is_not_acknowledged_by_live_or_resync_ingestion() {
    let dir = tempfile::tempdir().unwrap();
    let db = crate::system::Directories::with_base_dir(dir.path().to_path_buf())
        .unwrap()
        .database_path();
    std::fs::write(
        db.parent()
            .unwrap()
            .join(crate::system::keyring::NO_KEYRING_MARKER),
        b"",
    )
    .unwrap();
    let client = crate::client::ChatterClient::builder()
        .server_url("https://localhost:1")
        .data_dir(dir.path().to_str().unwrap())
        .build()
        .unwrap();
    let inbound = ChannelInbound::new(
        client.messages().clone(),
        client.attachments().clone(),
        client.users().clone(),
    );
    let channel = ChannelId::new();
    let message = chat_v1::Message {
        id: crate::domain::MessageId::new().to_string(),
        channel_id: channel.to_string(),
        author_id: crate::domain::UserId::new().to_string(),
        timestamp: 1000,
        content: Some(chat_v1::message::Content {
            r#type: Some(chat_v1::message::content::Type::Text("retry me".into())),
        }),
        ..Default::default()
    };
    inbound
        .messages
        .database
        .connection()
        .unwrap()
        .execute_batch("PRAGMA query_only=ON")
        .unwrap();
    assert_eq!(
        inbound.ingest(&message, channel, false).await,
        Ingested::RetryableFailure
    );
    assert!(inbound.ingest_resynced(&message, channel).await.is_err());
    inbound
        .messages
        .database
        .connection()
        .unwrap()
        .execute_batch("PRAGMA query_only=OFF")
        .unwrap();
    assert_eq!(
        inbound.ingest(&message, channel, false).await,
        Ingested::Stored
    );
    assert!(
        inbound
            .messages
            .get_message_local(&message.id.parse().unwrap())
            .unwrap()
            .is_some()
    );
}

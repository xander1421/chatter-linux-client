//! P8: a retained ratchet must never cross a local device-address replacement.
use chatter_core::{
    client::ChatterClient,
    domain::{
        ids::{ChannelId, UserId},
        message::ConversationType,
    },
    events::DomainEvent,
    state::ActiveConversation,
};
use std::{sync::Arc, time::Duration};

// A cold Tor connection may spend 120s bootstrapping and 90s dialing;
// allow 30s more for inbox polling and the mailbox handshake.
const DELIVERY_BUDGET: Duration = Duration::from_secs(120 + 90 + 30);

fn client(dir: &tempfile::TempDir) -> Arc<ChatterClient> {
    crate::common::headless(dir.path());
    ChatterClient::builder()
        .server_url("https://localhost:50051")
        .data_dir(dir.path().to_str().unwrap())
        .build()
        .unwrap()
}

async fn receive(rx: &mut tokio::sync::broadcast::Receiver<DomainEvent>, text: &str) {
    tokio::time::timeout(DELIVERY_BUDGET, async {
        loop {
            if let Ok(DomainEvent::MessageReceived { message }) = rx.recv().await {
                if message.as_text() == Some(text) {
                    break;
                }
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("receiver must obtain plaintext for phase {text}"));
}

#[tokio::test]
async fn shield_dm_after_fresh_signin_delivers_exactly_once() {
    crate::skip_if_no_server!();
    // Phase timings and pagination only: keep cryptographic payloads/keys out
    // of this isolated regression's diagnostics.
    let _ = tracing_subscriber::fmt()
        .with_env_filter("off,chatter_core::network::grpc::onion_transport=info,chatter_core::inbox::poller=info,chatter_core::inbox::streaming::dm_bootstrap_handler=info,chatter_core::services::message::dm_bootstrap=info")
        .with_test_writer()
        .try_init();
    let dirs = [tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap()];
    let clients = [client(&dirs[0]), client(&dirs[1])];
    // Random test-only credentials; never log them.
    let password = format!("P8!{}", uuid::Uuid::new_v4());
    let pin = format!("P8{}", uuid::Uuid::new_v4().simple());
    let names = [
        format!("p8a_{}", &uuid::Uuid::new_v4().simple().to_string()[..10]),
        format!("p8b_{}", &uuid::Uuid::new_v4().simple().to_string()[..10]),
    ];
    let mut users = Vec::<UserId>::new();
    let seed_dir = tempfile::tempdir().unwrap();
    let seed = client(&seed_dir);
    // Occupy device 1 so A first establishes its ratchet as device 2.
    seed.connect().await.unwrap();
    let seed_user = seed
        .register(&names[0], &password, "P8", "p8-seed", None, Some(&pin))
        .await
        .unwrap()
        .user_id;
    clients[0].connect().await.unwrap();
    let first_login = clients[0]
        .login(&names[0], &password, "p8-device-2", Some(&pin))
        .await
        .unwrap();
    assert_eq!(first_login.user_id, seed_user);
    assert_ne!(first_login.device_id, 1);
    users.push(first_login.user_id);
    seed.logout().await.unwrap();
    clients[0].prefetch_tokens(10).await.unwrap();
    for (c, name) in clients.iter().zip(&names).skip(1) {
        c.connect().await.unwrap();
        users.push(
            c.register(name, &password, "P8", "p8-test", None, Some(&pin))
                .await
                .unwrap()
                .user_id,
        );
        c.prefetch_tokens(10).await.unwrap();
    }
    clients[0]
        .send_friend_request(&users[1], None)
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_secs(2)).await;
    clients[1]
        .respond_friend_request(&users[0], true)
        .await
        .unwrap();
    let mut receiver = clients[1].subscribe_events();
    // Establish an acknowledged bidirectional ratchet before replacing A's
    // address. A subsequent Whisper must not be reused under its new address.
    clients[0]
        .send_text(
            ActiveConversation::ServerDm { peer_id: users[1] },
            "p8-before",
        )
        .await
        .unwrap();
    receive(&mut receiver, "p8-before").await;
    let mut sender_events = clients[0].subscribe_events();
    clients[1]
        .send_text(
            ActiveConversation::ServerDm { peer_id: users[0] },
            "p8-reply",
        )
        .await
        .unwrap();
    receive(&mut sender_events, "p8-reply").await;

    clients[0].logout().await.unwrap();
    let second_login = clients[0]
        .login(&names[0], &password, "p8-relogin", Some(&pin))
        .await
        .unwrap();
    assert_ne!(
        first_login.device_id, second_login.device_id,
        "regression must replace the local Signal address"
    );
    clients[0].set_anon_mode_enabled(true);
    let text = "p8-shield-after-signin-exactly-once";
    clients[0]
        .send_text(ActiveConversation::ServerDm { peer_id: users[1] }, text)
        .await
        .unwrap();
    receive(&mut receiver, text).await;
    tokio::time::timeout(DELIVERY_BUDGET, async {
        loop {
            let rows = clients[0]
                .get_messages_local_asc(
                    &ChannelId::from(users[1]),
                    &ConversationType::ServerDm,
                    None,
                    100,
                )
                .unwrap();
            if rows.iter().any(|m| {
                m.as_text() == Some(text)
                    && matches!(m.delivery_status.as_str(), "sent" | "delivered" | "read")
            }) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    })
    .await
    .expect("sender must leave sending state");
    tokio::time::sleep(Duration::from_secs(5)).await;
    let rows = clients[1]
        .get_messages_local_asc(
            &ChannelId::from(users[0]),
            &ConversationType::ServerDm,
            None,
            100,
        )
        .unwrap();
    assert_eq!(rows.iter().filter(|m| m.as_text() == Some(text)).count(), 1);
    assert!(
        rows.iter().all(|m| !matches!(
            m.content,
            chatter_core::domain::message::MessageContent::System(
                chatter_core::domain::message::SystemMessage::DeliveryIssue
            )
        )),
        "fresh device must not produce a decrypt-error placeholder"
    );
    // Recreate the RC116 journal on the sender: an earlier Shield bubble was
    // left unconfirmed after the remote response disappeared. It must retain
    // its exact identity without holding the next composed Shield message.
    let old = clients[0]
        .get_messages_local_asc(
            &ChannelId::from(users[1]),
            &ConversationType::ServerDm,
            None,
            100,
        )
        .unwrap()
        .into_iter()
        .find(|m| m.as_text() == Some(text))
        .unwrap();
    let db = clients[0].messages().database();
    let key = format!(
        "send.unresolved.v1:{}:{:?}",
        users[0],
        ActiveConversation::ServerDm { peer_id: users[1] }
    );
    db.connection().unwrap().execute(
        "UPDATE messages SET delivery_status='sending',failure_reason='Delivery unconfirmed' WHERE id=?1",
        rusqlite::params![old.id.to_string()],
    ).unwrap();
    db.connection()
        .unwrap()
        .execute(
            "INSERT INTO settings(key,value,updated_at) VALUES (?1,?2,?3)",
            rusqlite::params![
                key,
                serde_json::json!({
                    "attempt_id": old.id.to_string(), "text": text, "attachments": [],
                    "quote": null, "options": null, "message_id": old.id.to_string(),
                    "started": true, "process": "earlier-client"
                })
                .to_string(),
                chrono::Utc::now().to_rfc3339()
            ],
        )
        .unwrap();
    let next = "p21-shield-after-unconfirmed";
    clients[0]
        .send_text(ActiveConversation::ServerDm { peer_id: users[1] }, next)
        .await
        .unwrap();
    receive(&mut receiver, next).await;
    let rows = clients[1]
        .get_messages_local_asc(
            &ChannelId::from(users[0]),
            &ConversationType::ServerDm,
            None,
            100,
        )
        .unwrap();
    assert_eq!(rows.iter().filter(|m| m.as_text() == Some(next)).count(), 1);
    for c in clients {
        c.shutdown().await;
    }
}

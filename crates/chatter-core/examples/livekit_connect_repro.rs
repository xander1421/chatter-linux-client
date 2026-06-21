//! Isolated repro for the anon-DM call segfault.
//!
//! Spawns the same `RoomManager::connect` path the anon-DM accept flow
//! uses (E2EE GCM via a Signal-style shared key, auto-subscribe on),
//! against a local LiveKit dev server. Two processes can target the same
//! room to reproduce the 2-instances-on-one-host scenario.
//!
//! Env vars:
//!   LIVEKIT_URL       - default `ws://127.0.0.1:7880`
//!   LIVEKIT_API_KEY   - default `devkey`   (matches livekit dev defaults)
//!   LIVEKIT_API_SECRET- default `secret`
//!   ROOM_NAME         - default `anon-segfault-repro`
//!   IDENTITY          - participant identity; required if you want to run
//!                       a second instance against the same room.
//!   PUBLISH_MIC       - `1` to call publish_microphone after connect
//!                       (mirrors auto_publish_media); default off so we
//!                       can localize the crash to Room::connect vs ADM.
//!
//! Run two terminals:
//!     IDENTITY=alice cargo run --example livekit_connect_repro -p chatter-core
//!     IDENTITY=bob   cargo run --example livekit_connect_repro -p chatter-core

#[cfg(not(feature = "desktop-media"))]
fn main() {
    eprintln!("This example requires the `desktop-media` feature (enabled by default).");
}

#[cfg(feature = "desktop-media")]
fn main() {
    use std::time::Duration;
    use chatter_core::calls::RoomManager;
    use livekit_api::access_token::{AccessToken, VideoGrants};

    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,livekit=info,chatter_core=info")))
        .init();

    let url = std::env::var("LIVEKIT_URL")
        .unwrap_or_else(|_| "ws://127.0.0.1:7880".to_string());
    let api_key = std::env::var("LIVEKIT_API_KEY")
        .unwrap_or_else(|_| "devkey".to_string());
    let api_secret = std::env::var("LIVEKIT_API_SECRET")
        .unwrap_or_else(|_| "secret".to_string());
    let room_name = std::env::var("ROOM_NAME")
        .unwrap_or_else(|_| "anon-segfault-repro".to_string());
    let identity = std::env::var("IDENTITY")
        .unwrap_or_else(|_| format!("repro-{}", std::process::id()));
    let publish_mic = std::env::var("PUBLISH_MIC").ok().as_deref() == Some("1");

    let token = AccessToken::with_api_key(&api_key, &api_secret)
        .with_identity(&identity)
        .with_name(&identity)
        .with_ttl(Duration::from_secs(60 * 10))
        .with_grants(VideoGrants {
            room_join: true,
            room: room_name.clone(),
            can_publish: true,
            can_subscribe: true,
            can_publish_data: true,
            ..Default::default()
        })
        .to_jwt()
        .expect("mint LiveKit JWT");

    // Same shape as anon-DM: 32-byte E2EE key from a deterministic source so
    // both instances derive the same key. Use the room name's SHA-256.
    let encryption_key: Vec<u8> = {
        use sha2::{Digest, Sha256};
        Sha256::digest(room_name.as_bytes()).to_vec()
    };

    tracing::info!(%url, %room_name, %identity, publish_mic, "starting repro");

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");

    runtime.block_on(async move {
        let (manager, mut events) = RoomManager::new();
        tracing::info!("RoomManager::new() returned (audio_playback init passed)");

        // Spawn an event drainer so the channel doesn't backpressure-block.
        tokio::spawn(async move {
            while let Some(ev) = events.recv().await {
                tracing::info!(?ev, "RoomEvent");
            }
            tracing::info!("event channel closed");
        });

        tracing::info!("calling RoomManager::connect ...");
        match manager.connect(&url, &token, encryption_key).await {
            Ok(()) => tracing::info!("connect Ok — survived Room::connect"),
            Err(e) => {
                tracing::error!(error = %e, "connect failed");
                return;
            }
        }

        if publish_mic {
            tracing::info!("calling publish_microphone (mirrors auto_publish_media) ...");
            match manager.publish_microphone(None).await {
                Ok(()) => tracing::info!("publish_microphone Ok"),
                Err(e) => tracing::error!(error = %e, "publish_microphone failed"),
            }
        }

        // Stay connected so a second instance can join. Sleep ~30s.
        tracing::info!("sleeping 30s — bring up second instance now");
        tokio::time::sleep(Duration::from_secs(30)).await;

        tracing::info!("disconnecting");
        manager.disconnect().await;
        tracing::info!("done");
    });
}

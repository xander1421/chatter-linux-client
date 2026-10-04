//! Shield chosen before sign-in, end to end over real Tor (server OPEN-ITEMS §132).
//!
//! `ChatterClient::set_shield_before_login` is what the sign-in and
//! create-account screens call. This drives it the way the app does and checks
//! the owner requirement: the server never sees the device's IP. The client's
//! DIRECT server URL is a local listener that counts TCP accepts; the real
//! server is reachable only through its pinned `.onion`. Registration
//! (challenge + proof of work + Register + SVR backup + avatar upload + the
//! post-login loops), a sign-in from a second device (OPAQUE, SVR restore, key
//! upload) and a recovery-code reset must all succeed, the session must stay
//! in Shield, and the listener must see zero connections.
//!
//! Needs the `onion` feature, `CHATTER_ONION_ADDR` at build time and the dev
//! server started with `ONION_ENABLED=true`. From `linux-client/`:
//!
//! ```text
//! ../scripts/anchors.sh exec env CHATTER_ONION_ADDR=$(cat ../data/onion/hs/hostname) \
//!     INTEGRATION_TESTS=1 CHATTER_REQUIRE_SERVER=1 \
//!     cargo test -p chatter-core \
//!     --features testing,onion,$(cd .. && ./scripts/anchors.sh cargo-features) \
//!     --test shield_onboarding -- --nocapture --test-threads=1
//! ```

mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chatter_core::client::ChatterClient;
use rand::RngCore;

const PASSWORD: &str = "TestPass123!";
const PIN: &str = "Kf8mq2xpZ";
/// Tor bootstrap plus a cold onion-service rendezvous can take a while.
const ONION_TIMEOUT: Duration = Duration::from_secs(180);

/// A local "direct server" that only counts who dials it.
async fn direct_listener() -> (String, Arc<AtomicUsize>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("https://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let accepts = Arc::new(AtomicUsize::new(0));
    let counter = accepts.clone();
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((sock, peer)) = listener.accept().await {
            eprintln!("DIRECT CONNECTION from {peer}");
            counter.fetch_add(1, Ordering::SeqCst);
            held.push(sock);
        }
    });
    (url, accepts)
}

/// A fresh client with Shield on before it has said a word to anyone.
fn shielded_client(direct_url: &str) -> (Arc<ChatterClient>, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    common::headless(dir.path());
    let client = ChatterClient::builder()
        .server_url(direct_url)
        .data_dir(dir.path().to_str().unwrap())
        .build()
        .expect("build client");
    assert!(client.shield_available(), "build with --features onion and CHATTER_ONION_ADDR");
    client.set_shield_before_login(true).expect("Shield before sign-in");
    (client, dir)
}

/// Connect over onion, retrying a few circuits: a hidden-service rendezvous
/// fails now and then over real Tor, which is the network, not the code.
async fn connect_over_onion(client: &ChatterClient) {
    let t = Instant::now();
    for attempt in 1..=3 {
        match tokio::time::timeout(ONION_TIMEOUT, client.connect()).await {
            Ok(Ok(())) => {
                eprintln!("onion connected in {:?} (attempt {attempt})", t.elapsed());
                return;
            }
            Ok(Err(e)) => eprintln!("onion connect attempt {attempt} failed: {e}"),
            Err(_) => eprintln!("onion connect attempt {attempt} timed out"),
        }
    }
    panic!("could not reach the onion service in 3 attempts");
}

fn assert_in_shield(client: &ChatterClient, accepts: &AtomicUsize, step: &str) {
    assert!(client.anon_mode_enabled(), "{step}: the session left Shield");
    assert!(client.endpoint().contains(".onion"), "{step}: transport left onion: {}", client.endpoint());
    assert_eq!(accepts.load(Ordering::SeqCst), 0, "{step}: the server was dialled directly");
}

#[tokio::test(flavor = "multi_thread")]
async fn shield_onboarding_never_dials_direct() {
    skip_if_no_server!();
    let base = std::env::temp_dir().join("chatter-test-arti");
    chatter_core::network::grpc::onion_transport::set_tor_directories(
        base.join("state"),
        base.join("cache"),
    );
    let (direct_url, accepts) = direct_listener().await;

    // ── Create account ────────────────────────────────────────────────
    let (alice, _alice_dir) = shielded_client(&direct_url);
    connect_over_onion(&alice).await;
    let mut b = [0u8; 4];
    rand::rng().fill_bytes(&mut b);
    let nick = format!("shield{:08x}", u32::from_le_bytes(b));
    let avatar = image_bytes();
    let t = Instant::now();
    let registered = tokio::time::timeout(
        ONION_TIMEOUT,
        alice.register(&nick, PASSWORD, "Shield", "shield-onboarding", Some(&avatar), Some(PIN)),
    )
    .await
    .expect("register over onion timed out")
    .expect("register over onion");
    eprintln!("registered {nick} over onion in {:?}", t.elapsed());
    // The post-login loops (inbox poll, streams, prekey check, presence,
    // avatar upload) get time to run; none may go direct.
    tokio::time::sleep(Duration::from_secs(15)).await;
    assert_in_shield(&alice, &accepts, "after registration");

    // ── Sign in on a second device ────────────────────────────────────
    let (bob_device, _bob_dir) = shielded_client(&direct_url);
    connect_over_onion(&bob_device).await;
    tokio::time::timeout(
        ONION_TIMEOUT,
        bob_device.login(&nick, PASSWORD, "shield-onboarding-2", Some(PIN)),
    )
    .await
    .expect("sign-in over onion timed out")
    .expect("sign-in over onion");
    tokio::time::sleep(Duration::from_secs(10)).await;
    assert_in_shield(&bob_device, &accepts, "after sign-in");

    // ── Recovery code ─────────────────────────────────────────────────
    let code = registered
        .recovery_codes
        .as_ref()
        .and_then(|c| c.first().cloned())
        .expect("registration returned recovery codes");
    let (recovering, _rec_dir) = shielded_client(&direct_url);
    connect_over_onion(&recovering).await;
    tokio::time::timeout(
        ONION_TIMEOUT,
        recovering.use_recovery_code(&nick, &code, "NewPass456!", "Zq9wl3Lmx"),
    )
    .await
    .expect("recovery over onion timed out")
    .expect("recovery over onion");
    assert_in_shield(&recovering, &accepts, "after recovery");

    // Teardown: let the logged-in clients stop; still nothing direct.
    alice.shutdown().await;
    bob_device.shutdown().await;
    assert_eq!(accepts.load(Ordering::SeqCst), 0, "a direct connection was made");
}

/// A tiny valid PNG, so registration exercises the avatar path.
fn image_bytes() -> Vec<u8> {
    let img = image::RgbaImage::from_pixel(8, 8, image::Rgba([200, 30, 30, 255]));
    let mut out = std::io::Cursor::new(Vec::new());
    img.write_to(&mut out, image::ImageFormat::Png).unwrap();
    out.into_inner()
}

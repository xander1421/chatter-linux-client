//! Onion sign-up proof of work, end to end over real Tor (server OPEN-ITEMS §130).
//!
//! The server charges `Register` a proof of work when the caller arrives
//! through its onion service, and charges nothing on clearnet. These
//! tests dial the dev server's `.onion` with the client's own Arti transport —
//! the same `TransportMode::Onion` path Shield uses — and prove:
//!
//! * a challenge fetched over onion carries a nonzero difficulty;
//! * a full client switched to onion BEFORE registering signs up (which the
//!   server only allows with a valid solution), and logs its solve time;
//! * a `Register` over onion with an absent solution is refused for the
//!   proof of work.
//!
//! Written against the registration path, not the puzzle: nothing here
//! solves or checks a solution itself (the full client does that), so the
//! tests outlive a change of puzzle.
//!
//! Needs the `onion` feature and `CHATTER_ONION_ADDR` set at build time (the
//! pinned address, `config::PINNED_ONION_ADDR`; the dev server writes it to
//! `data/onion/hs/hostname` when started with `ONION_ENABLED=true`). From
//! `linux-client/`:
//!
//! ```text
//! ../scripts/anchors.sh exec env CHATTER_ONION_ADDR=$(cat ../data/onion/hs/hostname) \
//!     INTEGRATION_TESTS=1 CHATTER_REQUIRE_SERVER=1 \
//!     cargo test -p chatter-core \
//!     --features testing,onion,$(cd .. && ./scripts/anchors.sh cargo-features) \
//!     --test onion_signup_pow -- --nocapture --test-threads=1
//! ```
//!
//! `CHATTER_EXPECT_ONION_POW_BITS`, if set, pins the exact difficulty expected.

// Shared with tests/it.rs, which uses nearly all of it and stays the
// dead-code check for it; this target uses a subset.
#[allow(dead_code, unused_imports)]
mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use chatter_core::client::ChatterClient;
use chatter_core::crypto::{auth_crypto, opaque};
use chatter_core::network::grpc::client_pool::TransportMode;
use chatter_core::network::grpc::{AuthClient, GrpcClientPool};
use chatter_core::proto::chat_v1::{RegisterRequest, auth_service_client::AuthServiceClient};
use rand::RngCore;

const SERVER_URL: &str = "https://localhost:50051";
const PASSWORD: &str = "TestPass123!";
/// Tor bootstrap plus a cold onion-service rendezvous can take a while.
const ONION_TIMEOUT: Duration = Duration::from_secs(180);

/// An onion challenge must ask for work; if the run names the exact
/// difficulty, it must be that.
fn check_difficulty(bits: u32) {
    assert!(
        bits > 0,
        "a challenge over onion asked for no proof of work"
    );
    if let Ok(v) = std::env::var("CHATTER_EXPECT_ONION_POW_BITS") {
        assert_eq!(
            bits,
            v.parse::<u32>().expect("CHATTER_EXPECT_ONION_POW_BITS")
        );
    }
}

/// The pinned onion as a pool URL, normalised as `app/infrastructure.rs` does.
fn onion_url() -> String {
    let onion = chatter_core::config::PINNED_ONION_ADDR
        .expect("build with CHATTER_ONION_ADDR=<dev server .onion>")
        .trim();
    if onion.contains("://") {
        onion.to_string()
    } else {
        format!("http://{onion}:443")
    }
}

/// Arti state/cache in one persistent place, so reruns skip the full
/// directory download. First call wins process-wide (the client builder's own
/// call then does nothing), which is also why these tests share one directory.
fn tor_dirs() {
    let base = std::env::temp_dir().join("chatter-test-arti");
    chatter_core::network::grpc::onion_transport::set_tor_directories(
        base.join("state"),
        base.join("cache"),
    );
}

/// Everything logged, kept so a test can assert on what the client logged.
static LOGS: std::sync::Mutex<Vec<u8>> = std::sync::Mutex::new(Vec::new());

struct Tee;
impl std::io::Write for Tee {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        LOGS.lock().unwrap().extend_from_slice(buf);
        std::io::stderr().write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        std::io::stderr().flush()
    }
}

fn init_logs() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            "chatter_core::services::auth_service=info,chatter_core::network::grpc=info",
        )
        .with_ansi(false)
        .with_writer(|| Tee)
        .try_init();
}

fn unique_nickname() -> String {
    let mut b = [0u8; 4];
    rand::rng().fill_bytes(&mut b);
    format!("powonion{:08x}", u32::from_le_bytes(b))
}

fn random32() -> [u8; 32] {
    let mut b = [0u8; 32];
    rand::rng().fill_bytes(&mut b);
    b
}

async fn onion_pool() -> GrpcClientPool {
    let pool = GrpcClientPool::new(SERVER_URL).expect("pool");
    pool.set_onion_address(Some(onion_url()));
    pool.set_transport_mode(TransportMode::Onion)
        .expect("switch pool to onion");
    // A rendezvous to a hidden service fails now and then over real Tor;
    // that is the network, not the code under test, so try a few circuits.
    let t = Instant::now();
    for attempt in 1..=3 {
        match tokio::time::timeout(ONION_TIMEOUT, pool.connect()).await {
            Ok(Ok(())) => {
                eprintln!("onion connected in {:?} (attempt {attempt})", t.elapsed());
                return pool;
            }
            Ok(Err(e)) => eprintln!("onion connect attempt {attempt} failed: {e}"),
            Err(_) => eprintln!("onion connect attempt {attempt} timed out"),
        }
    }
    panic!("could not reach the onion service in 3 attempts");
}

/// A registration challenge over `pool` for a fresh nickname and `identity_key`
/// (raw 32-byte Curve25519), with the OPAQUE record it makes possible.
async fn onion_challenge(
    pool: &GrpcClientPool,
    identity_key: &[u8],
) -> (
    AuthClient,
    String,
    Vec<u8>,
    chatter_core::proto::chat_v1::RequestRegistrationChallengeResponse,
) {
    let mut auth = AuthClient::new(pool.channel(), pool.auth_clone());
    let nick = unique_nickname();
    let reg = opaque::registration_start(PASSWORD).unwrap();
    let challenge = auth
        .request_registration_challenge(
            &auth_crypto::login_identifier(&nick),
            identity_key,
            &reg.request,
        )
        .await
        .expect("challenge over onion");
    check_difficulty(
        challenge
            .challenge
            .as_ref()
            .expect("challenge token")
            .difficulty,
    );
    let record =
        opaque::registration_finish(reg, PASSWORD, &challenge.opaque_registration_response)
            .expect("OPAQUE registration record");
    (auth, nick, record, challenge)
}

#[tokio::test(flavor = "multi_thread")]
async fn onion_register_solves_the_onion_pow() {
    skip_if_no_server!();
    init_logs();
    tor_dirs();

    // The difficulty a challenge over onion carries, fetched on the same
    // transport the client below registers over.
    let pool = onion_pool().await;
    let (_, _, _, challenge) = onion_challenge(&pool, &random32()).await;
    let bits = challenge
        .challenge
        .as_ref()
        .expect("challenge token")
        .difficulty;
    pool.disconnect();

    // A real client, on onion before it has said a word to the server.
    let dir = tempfile::tempdir().unwrap();
    common::headless(dir.path());
    let client: Arc<ChatterClient> = ChatterClient::builder()
        .server_url(SERVER_URL)
        .data_dir(dir.path().to_str().unwrap())
        .build()
        .expect("build client");
    client.set_anon_mode_enabled(true);
    // Set from the pool's actual transport after the switch (client/mod.rs).
    assert!(
        chatter_core::network::anon_http::ANON_HTTP_ACTIVE
            .load(std::sync::atomic::Ordering::Acquire),
        "client pool did not switch to onion"
    );

    // Match the preliminary pool's bounded initial-connect retries. Keep the
    // 180s caller cutoff and all registration/PoW assertions strict: retry only
    // connection establishment, never registration or a failed PoW assertion.
    let mut connected = false;
    for attempt in 1..=3 {
        match tokio::time::timeout(ONION_TIMEOUT, client.connect()).await {
            Ok(Ok(())) => {
                connected = true;
                break;
            }
            Ok(Err(e)) => eprintln!("client onion connect attempt {attempt} failed: {e}"),
            Err(_) => eprintln!("client onion connect attempt {attempt} timed out"),
        }
    }
    assert!(connected, "client could not reach onion in 3 attempts");
    let nick = unique_nickname();
    let t = Instant::now();
    // The server refuses an onion Register without valid work (the negative
    // test below), so success here means the client solved it.
    let result = tokio::time::timeout(
        ONION_TIMEOUT,
        client.register(
            &nick,
            PASSWORD,
            "Onion PoW",
            "onion-pow-test",
            None,
            Some("Kf8mq2xpZ"),
        ),
    )
    .await
    .expect("register over onion timed out")
    .expect("register over onion");
    // The client's own log of the challenge it solved: only an onion
    // challenge asks for work (clearnet is 0).
    let logs = String::from_utf8_lossy(&LOGS.lock().unwrap()).into_owned();
    let line = logs
        .lines()
        .rfind(|l| l.contains("registration proof of work solved"))
        .expect("client did not log its proof-of-work solve");
    assert!(
        line.contains(&format!("pow_bits={bits}")),
        "client solved the wrong difficulty: {line}"
    );
    assert!(line.contains("solve_ms="), "solve time not logged: {line}");
    eprintln!(
        "registered {nick} over onion as {:?} in {:?} (incl. OPAQUE, prekeys, PoW at {bits})",
        result.user_id,
        t.elapsed()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn onion_register_without_pow_is_refused() {
    skip_if_no_server!();
    init_logs();
    tor_dirs();
    let pool = onion_pool().await;
    let (bundle, identity_key) = common::generate_prekey_bundle();
    let (_, nick, record, challenge) = onion_challenge(&pool, &identity_key[1..]).await;

    // Everything `Register` checks before the challenge is valid (request
    // validation, username proof, required fields). The challenge check tests the work before the
    // (junk) signature, so a refusal naming the work is the work check firing.
    // The solution is the proto default: an absent one.
    // Raw proto client: the AuthClient wrapper maps PermissionDenied to a
    // bare variant and drops the server's reason, which is what this asserts.
    let (lookup, proof) = auth_crypto::username_claim(&format!("{nick}.42")).unwrap();
    let mut raw = AuthServiceClient::new(pool.channel());
    let request = RegisterRequest {
        username: auth_crypto::login_identifier(&nick),
        opaque_registration_record: record,
        svr_public_key: identity_key[1..].to_vec(),
        // A real ciphertext: the handler refuses anything outside the
        // display-name padding buckets before the challenge is looked at.
        display_name: {
            use base64::Engine as _;
            let ct =
                chatter_core::crypto::profile_crypto::encrypt_display_name(&random32(), "junk")
                    .unwrap();
            base64::engine::general_purpose::STANDARD.encode(ct)
        },
        device_info: "onion-pow-negative".into(),
        request_id: uuid::Uuid::new_v4().to_string(),
        device_auth_token: random32().to_vec(),
        registration_lock: vec![0u8; 32],
        prekey_bundle: Some(bundle),
        challenge_token: challenge
            .challenge
            .as_ref()
            .expect("challenge token")
            .token
            .clone(),
        challenge_response: vec![0u8; 64],
        username_lookup_hash: lookup.to_vec(),
        username_proof: proof,
        unidentified_access_key: vec![0u8; 16],
        // The solution field(s) left at their default: no proof of work.
        ..Default::default()
    };
    let refused = match raw.register(request).await {
        Err(status) => status,
        Ok(stream) => {
            let mut stream = stream.into_inner();
            loop {
                match stream.message().await {
                    Err(status) => break status,
                    Ok(Some(event)) => eprintln!("unexpected event: {event:?}"),
                    Ok(None) => panic!("an onion Register without proof of work was accepted"),
                }
            }
        }
    };
    eprintln!(
        "absent solution over onion: {:?}: {}",
        refused.code(),
        refused.message()
    );
    assert_eq!(refused.code(), tonic::Code::PermissionDenied, "{refused:?}");
    assert!(
        refused.message().contains("proof of work"),
        "refused, but not for the work: {refused:?}"
    );
}

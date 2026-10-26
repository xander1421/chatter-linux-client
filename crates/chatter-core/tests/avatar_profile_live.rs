//! Focused RC80 own-avatar facade proof against the development server.
//! Uses only newly registered disposable users and normal authenticated APIs;
//! no server dumps, credential extraction, private account fixtures or UI/device work.
//! Sabotage controls: skipping upload fails the cache-busted own fetch; dropping
//! peer decryption/key exchange fails the byte-exact peer assertion; removing
//! owner/wire-budget admission changes the positive profile and fails refusal checks.
#[allow(dead_code, unused_imports)]
mod common;

use chatter_core::{client::ChatterClient, domain::ids::UserId, state::ActiveConversation};
use futures::FutureExt;
use std::{
    sync::{Arc, atomic::AtomicBool},
    time::{Duration, Instant},
};

const SERVER: &str = "https://localhost:50051";
// Public test-only values, used solely for accounts this fixture creates.
const PASSWORD: &str = "AvatarLiveTest123!";
const PIN: &str = "Av4tarLive!9x";
const WAIT: Duration = Duration::from_secs(90);

fn client(path: &std::path::Path) -> Arc<ChatterClient> {
    common::headless(path);
    ChatterClient::builder()
        .server_url(SERVER)
        .data_dir(path.to_str().unwrap())
        .build()
        .unwrap()
}
async fn register(client: &Arc<ChatterClient>, label: &str) -> Result<UserId, String> {
    client
        .connect()
        .await
        .map_err(|e| format!("fixture connect failed: {e}"))?;
    let username = common::unique_username();
    let response = client
        .register(
            &username,
            PASSWORD,
            label,
            "avatar-live-fixture",
            None,
            Some(PIN),
        )
        .await
        .map_err(|e| format!("fixture registration failed: {e}"))?;
    Ok(response.user_id)
}

async fn befriend(a: &Arc<ChatterClient>, b: &Arc<ChatterClient>, aid: UserId, bid: UserId) {
    a.send_friend_request(&bid, Some("Owned avatar live fixture"))
        .await
        .unwrap();
    // As in validated::befriend: request persistence is asynchronous Temporal
    // work, so observe actual pending/accepted state rather than a fixed sleep.
    let deadline = Instant::now() + WAIT;
    loop {
        let pending = b.list_pending_friend_requests().await.unwrap();
        if pending.iter().any(|f| f.addressee_id == aid) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "fixture friendship request never became visible"
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    loop {
        match b.respond_friend_request(&aid, true).await {
            Ok(_) => break,
            Err(e) if e.to_string().contains("no friend request") && Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            Err(e) => panic!("fixture friend acceptance failed: {e}"),
        }
    }
    loop {
        let a_ready = a
            .list_friends_with_profiles()
            .await
            .unwrap()
            .iter()
            .any(|(f, _)| f.addressee_id == bid);
        let b_ready = b
            .list_friends_with_profiles()
            .await
            .unwrap()
            .iter()
            .any(|(f, _)| f.addressee_id == aid);
        if a_ready && b_ready {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "fixture friendship did not become mutual"
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    a.send_text(
        ActiveConversation::ServerDm { peer_id: bid },
        "Owned avatar profile-key carrier",
    )
    .await
    .unwrap();
    let deadline = Instant::now() + WAIT;
    loop {
        let profile = b.get_user_profile(&aid).await.unwrap();
        if profile.display_name == "Avatar live A" {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "new peer did not receive the normal encrypted profile-key exchange"
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

fn normalized_avatar() -> Vec<u8> {
    let image = image::RgbaImage::from_fn(37, 21, |x, y| {
        image::Rgba([(x * 5) as u8, (y * 11) as u8, ((x + y) * 3) as u8, 255])
    });
    let mut raw = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(image)
        .write_to(&mut raw, image::ImageFormat::Png)
        .unwrap();
    let normalized =
        chatter_core::media::ImageProcessor::process_avatar_from_memory(&raw.into_inner()).unwrap();
    assert!(!normalized.is_empty());
    assert!(normalized.len() + 28 <= chatter_core::domain::anon_dm_init::MAX_AVATAR_SIZE);
    assert_eq!(image::load_from_memory(&normalized).unwrap().width(), 512);
    normalized
}

async fn proof(a: &Arc<ChatterClient>, b: &Arc<ChatterClient>, aid: UserId, bid: UserId) {
    befriend(a, b, aid, bid).await;
    assert!(
        a.my_avatar().is_none(),
        "fresh user must have no previous avatar"
    );
    assert!(
        b.get_user_profile(&aid)
            .await
            .unwrap()
            .avatar_bytes
            .is_none()
    );
    let avatar = normalized_avatar();
    let revision = a.auth_snapshot().session_revision;
    let name = "RC80 normalized live avatar";
    let saved = a
        .save_own_profile_in_session(
            aid,
            revision,
            Arc::new(AtomicBool::new(true)),
            Some(avatar.clone()),
            name.into(),
            None,
        )
        .await
        .expect("actual scoped avatar Save failed");
    assert_eq!(saved.user_id, aid);
    assert_eq!(saved.display_name, name);
    assert_eq!(saved.avatar_bytes.as_deref(), Some(avatar.as_slice()));
    assert_eq!(a.my_avatar().as_deref(), Some(avatar.as_slice()));
    let local = a.get_profile_local(&aid).unwrap();
    assert_eq!(local.display_name, name);
    assert_eq!(local.avatar_bytes.as_deref(), Some(avatar.as_slice()));
    assert_eq!(a.auth_snapshot().display_name.as_deref(), Some(name));
    eprintln!("avatar-live: actual scoped Save returned/persisted exact normalized image and name");

    // Cache-bust only this owned fixture's local AVATAR via the normal facade.
    // Own get_user_profile otherwise falls back to AVATAR when server image is
    // absent. Restoring these nonempty bytes from an empty local value proves
    // the authenticated server fetch actually carried decryptable ciphertext.
    a.set_avatar_bytes(&[]).unwrap();
    assert_eq!(a.my_avatar(), Some(Vec::new()));
    let fetched = a.get_user_profile(&aid).await.unwrap();
    assert_eq!(fetched.user_id, aid);
    assert_eq!(fetched.avatar_bytes.as_deref(), Some(avatar.as_slice()));
    assert_eq!(fetched.display_name, name);
    assert_eq!(a.my_avatar().as_deref(), Some(avatar.as_slice()));
    eprintln!("avatar-live: cache-busted authenticated own fetch restored exact server avatar");

    let deadline = Instant::now() + WAIT;
    loop {
        let peer = b.get_user_profile(&aid).await.unwrap();
        if peer.display_name == name && peer.avatar_bytes.as_deref() == Some(avatar.as_slice()) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "new friend did not fetch/decrypt the exact updated avatar and name"
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    eprintln!(
        "avatar-live: new peer fetched/decrypted exact avatar and name via normal friendship/profile-key exchange"
    );

    // A's captured edit submitted against B must refuse before touching B or
    // issuing A's update. Same-account obsolete revision is a separate guard.
    let b_before = b.get_user_profile(&bid).await.unwrap();
    let b_avatar_before = b.my_avatar();
    assert!(
        b.save_own_profile_in_session(
            aid,
            revision,
            Arc::new(AtomicBool::new(true)),
            Some(avatar.clone()),
            "wrong owner must not persist".into(),
            None
        )
        .await
        .is_err()
    );
    assert_eq!(b.auth_snapshot().current_user_id, Some(bid));
    assert_eq!(b.my_avatar(), b_avatar_before);
    assert_eq!(b.get_user_profile(&bid).await.unwrap(), b_before);
    assert!(
        a.save_own_profile_in_session(
            aid,
            revision.wrapping_add(1),
            Arc::new(AtomicBool::new(true)),
            Some(avatar.clone()),
            "wrong revision must not persist".into(),
            None
        )
        .await
        .is_err()
    );
    // Boundary bytes are <= setter raw cap but exceed encrypted budget by1.
    // Moving admission after the setter would overwrite the positive avatar.
    let invalid = vec![0; chatter_core::domain::anon_dm_init::MAX_AVATAR_SIZE - 27];
    let error = a
        .save_own_profile_in_session(
            aid,
            revision,
            Arc::new(AtomicBool::new(true)),
            Some(invalid),
            "over budget must not persist".into(),
            None,
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("picture is too large"));
    assert_eq!(a.auth_snapshot().current_user_id, Some(aid));
    assert_eq!(a.auth_snapshot().session_revision, revision);
    assert_eq!(a.my_avatar().as_deref(), Some(avatar.as_slice()));
    assert_eq!(a.get_profile_local(&aid).unwrap().display_name, name);
    assert_eq!(
        a.get_user_profile(&aid)
            .await
            .unwrap()
            .avatar_bytes
            .as_deref(),
        Some(avatar.as_slice())
    );
    assert_eq!(b.get_user_profile(&aid).await.unwrap().display_name, name);
    eprintln!(
        "avatar-live: wrong owner/revision and invalid encrypted budget refused; current owners preserved"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scoped_avatar_save_round_trips_on_two_disposable_live_users() {
    // A required run cannot return early and be reported as a vacuous pass.
    assert_eq!(std::env::var("INTEGRATION_TESTS").as_deref(), Ok("1"));
    assert_eq!(std::env::var("CHATTER_REQUIRE_SERVER").as_deref(), Ok("1"));
    assert_eq!(std::env::var("CHATTER_NO_KEYRING").as_deref(), Ok("1"));
    assert_eq!(
        std::env::var("DBUS_SESSION_BUS_ADDRESS").as_deref(),
        Ok("unix:path=/tmp/chatter-no-secret-service")
    );
    let fixture = tempfile::tempdir().unwrap();
    let a = client(&fixture.path().join("a"));
    let b = client(&fixture.path().join("b"));
    let mut created = Vec::new();
    let result = std::panic::AssertUnwindSafe(async {
        let aid = register(&a, "Avatar live A").await.unwrap();
        created.push(a.clone());
        if let Err(error) = a.start().await {
            assert!(
                matches!(error, chatter_core::client::ClientError::AlreadyStarted),
                "fixture A startup failed: {error}"
            );
        }
        let bid = register(&b, "Avatar live B").await.unwrap();
        created.push(b.clone());
        if let Err(error) = b.start().await {
            assert!(
                matches!(error, chatter_core::client::ClientError::AlreadyStarted),
                "fixture B startup failed: {error}"
            );
        }
        tokio::time::timeout(Duration::from_secs(240), proof(&a, &b, aid, bid))
            .await
            .expect("bounded avatar live proof expired");
    })
    .catch_unwind()
    .await;
    let mut cleanup_ok = true;
    for user in created.iter().rev() {
        let deleted = tokio::time::timeout(WAIT, user.delete_account(PASSWORD)).await;
        cleanup_ok &= matches!(deleted, Ok(Ok(())));
        user.shutdown().await;
    }
    // Also shut down any built-but-not-registered clients; no account mutation.
    a.shutdown().await;
    b.shutdown().await;
    eprintln!(
        "avatar-live: fixture accounts registered={}, supported API cleanup={cleanup_ok}",
        created.len()
    );
    assert!(
        cleanup_ok,
        "supported deletion of newly created fixture accounts did not complete"
    );
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
    assert_eq!(created.len(), 2, "actual proof requires two new users");
    eprintln!("AVATAR_LIVE_PROOF_COMPLETED users=2 cleanup=2");
}

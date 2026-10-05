//! Integration tests for AuthClient.
//!
//! Tests authentication, registration, session management, and device management
//! against a running server.

use crate::{common, skip_if_no_server};

use tokio_stream::StreamExt;

use chatter_core::error::NetworkError;
use chatter_core::network::grpc::AuthClient;
use chatter_core::proto::chat_v1::auth_event;

use common::*;

// ============================================================================
// Registration Tests
// ============================================================================

#[tokio::test]
async fn test_register_user_stream() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let mut auth_client = AuthClient::new(pool.channel(), pool.auth_clone());

    let username = unique_username();
    let device_auth_token = random_device_auth_token();
    let profile_key = random_profile_key();
    let display_name = format!("Test User {}", &username[10..18]);
    // Encrypt display name with profile key (Phase 79 privacy)
    let encrypted_display_name = encrypt_test_display_name(&profile_key, &display_name);
    // Derive UAK from profile key for sealed sender
    let uak = chatter_core::crypto::derive_uak(&profile_key);

    // Generate real Signal Protocol keys
    use chatter_core::proto::chat_v1::signal_pre_key_bundle::{PreKeyRecord, SignedPreKeyRecord};
    use chatter_core::proto::chat_v1::SignalPreKeyBundle;
    use libsignal_protocol::{IdentityKeyPair, KeyPair};

    let identity_key_pair = IdentityKeyPair::generate(&mut rand::rng());
    let identity_public_key = identity_key_pair.identity_key().serialize();

    let signed_prekey_pair = KeyPair::generate(&mut rand::rng());
    let signed_prekey_signature = identity_key_pair
        .private_key()
        .calculate_signature(&signed_prekey_pair.public_key.serialize(), &mut rand::rng())
        .expect("Signature should succeed");

    let one_time_prekey_pair = KeyPair::generate(&mut rand::rng());

    let prekey_bundle = SignalPreKeyBundle {
        identity_key: identity_public_key.as_ref().to_vec(),
        registration_id: rand::random::<u32>() % 16380 + 1,
        device_id: 1,
        signed_prekey: Some(SignedPreKeyRecord {
            id: 1,
            public_key: signed_prekey_pair.public_key.serialize().to_vec(),
            signature: signed_prekey_signature.into_vec(),
            timestamp: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64,
        }),
        one_time_prekey: Some(PreKeyRecord {
            id: 1,
            public_key: one_time_prekey_pair.public_key.serialize().to_vec(),
        }),
        kyber_prekey: None,
        unrestricted_unidentified_access: Some(false),
    };

    let password = random_password();
    let username_hash_hex = chatter_core::crypto::auth_crypto::login_identifier(&username);
    // Discovery hash + the ZK proof the server verifies it against, over a
    // Signal-grammar `nickname.discriminator` (as `register_test_user` does).
    let full_username = chatter_core::crypto::auth_crypto::username_candidates(&username)
        .expect("test nickname is Signal-grammar")
        .remove(0);
    let (username_lookup_hash, username_proof) =
        chatter_core::crypto::auth_crypto::username_claim(&full_username)
            .expect("candidate is Signal-grammar");

    // Server now requires a registration challenge proving identity-key
    // possession, and the OPAQUE registration rides it. Mirror what
    // `register_test_user` does: request a nonce with the RegistrationRequest
    // and sign the domain-separated message (registration_challenge_message).
    let identity_public_key_stripped = &identity_public_key[1..]; // strip 0x05 type byte
    let opaque_reg = chatter_core::crypto::opaque::registration_start(&password).unwrap();
    let challenge = assert_ok(
        auth_client
            .request_registration_challenge(
                &username_hash_hex,
                identity_public_key_stripped,
                &opaque_reg.request,
            )
            .await,
    );
    let opaque_record = chatter_core::crypto::opaque::registration_finish_async(
        opaque_reg,
        &password,
        challenge.opaque_registration_response.clone(),
    )
    .await
    .expect("OPAQUE registration finishes against the server's response");
    assert_eq!(opaque_record.len(), 192);
    let challenge_response = identity_key_pair
        .private_key()
        .calculate_signature(
            &chatter_core::crypto::signal::identity::registration_challenge_message(
                &challenge.challenge_nonce,
            )
            .expect("server nonce must be REGISTRATION_CHALLENGE_NONCE_LEN bytes"),
            &mut rand::rng(),
        )
        .expect("XEdDSA challenge signing should succeed");

    let stream = assert_ok(
        auth_client
            .register(
                &username_hash_hex,
                &opaque_record,
                &encrypted_display_name,
                identity_public_key.as_ref(),
                "integration-test",
                &device_auth_token,
                &uak,
                Some(prekey_bundle),
                Vec::new(),
                &challenge.challenge.as_ref().expect("challenge token").token,
                &challenge_response,
                common::solve_registration_pow(&challenge),
                &[0u8; 32], // Empty registration lock for tests
                &username_lookup_hash,
                &username_proof,
            )
            .await,
    );

    tokio::pin!(stream);

    let mut received_completion = false;
    while let Some(result) = stream.next().await {
        let event = assert_ok(result);
        if let Some(payload) = event.payload {
            match payload {
                auth_event::Payload::UserRegistered(_) => {
                    // Progress event
                }
                auth_event::Payload::RegistrationComplete(result) => {
                    assert!(!result.user_id.is_empty(), "user_id should not be empty");
                    assert!(result.device_id > 0, "device_id should be > 0");
                    received_completion = true;
                }
                auth_event::Payload::RegistrationError(error) => {
                    panic!("Registration failed: {}", error.message);
                }
            }
        }
    }

    assert!(received_completion, "Should have received completion event");
}

#[tokio::test]
async fn test_register_and_wait() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let user = assert_ok(register_test_user(&pool).await);

    assert!(!user.user_id.is_empty());
    assert!(user.device_id > 0);
}

#[tokio::test]
async fn test_register_duplicate_username() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);

    // Register first user
    let user = assert_ok(register_test_user(&pool).await);

    // Generate real Signal Protocol keys for the duplicate attempt
    use chatter_core::proto::chat_v1::signal_pre_key_bundle::{PreKeyRecord, SignedPreKeyRecord};
    use chatter_core::proto::chat_v1::SignalPreKeyBundle;
    use libsignal_protocol::{IdentityKeyPair, KeyPair};

    let identity_key_pair = IdentityKeyPair::generate(&mut rand::rng());
    let identity_public_key = identity_key_pair.identity_key().serialize();

    let signed_prekey_pair = KeyPair::generate(&mut rand::rng());
    let signed_prekey_signature = identity_key_pair
        .private_key()
        .calculate_signature(&signed_prekey_pair.public_key.serialize(), &mut rand::rng())
        .expect("Signature should succeed");

    let one_time_prekey_pair = KeyPair::generate(&mut rand::rng());

    let prekey_bundle = SignalPreKeyBundle {
        identity_key: identity_public_key.as_ref().to_vec(),
        registration_id: rand::random::<u32>() % 16380 + 1,
        device_id: 1,
        signed_prekey: Some(SignedPreKeyRecord {
            id: 1,
            public_key: signed_prekey_pair.public_key.serialize().to_vec(),
            signature: signed_prekey_signature.into_vec(),
            timestamp: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64,
        }),
        one_time_prekey: Some(PreKeyRecord {
            id: 1,
            public_key: one_time_prekey_pair.public_key.serialize().to_vec(),
        }),
        kyber_prekey: None,
        unrestricted_unidentified_access: Some(false),
    };

    // Try to register with same username
    let mut auth_client = AuthClient::new(pool.channel(), pool.auth_clone());
    let dup_profile_key = random_profile_key();
    let dup_encrypted_name = encrypt_test_display_name(&dup_profile_key, "Duplicate User");
    let dup_uak = chatter_core::crypto::derive_uak(&dup_profile_key);
    // Same nickname => same login identifier (the `username` field the
    // registration-lock check keys on). The discovery hash must carry a
    // verifying username proof or the server rejects the request before it
    // reaches the registration-lock check.
    let dup_username_hash_hex = chatter_core::crypto::auth_crypto::login_identifier(&user.username);
    let (dup_username_lookup_hash, dup_username_proof) =
        chatter_core::crypto::auth_crypto::username_claim(&user.full_username)
            .expect("fixture full_username is Signal-grammar");

    // Registration challenge required by server.
    let identity_public_key_stripped = &identity_public_key[1..]; // strip 0x05 type byte
    let dup_reg = chatter_core::crypto::opaque::registration_start("another password").unwrap();
    let challenge = assert_ok(
        auth_client
            .request_registration_challenge(
                &dup_username_hash_hex,
                identity_public_key_stripped,
                &dup_reg.request,
            )
            .await,
    );
    let dup_record = chatter_core::crypto::opaque::registration_finish_async(
        dup_reg,
        "another password",
        challenge.opaque_registration_response.clone(),
    )
    .await
    .unwrap();
    let challenge_response = identity_key_pair
        .private_key()
        .calculate_signature(
            &chatter_core::crypto::signal::identity::registration_challenge_message(
                &challenge.challenge_nonce,
            )
            .expect("server nonce must be REGISTRATION_CHALLENGE_NONCE_LEN bytes"),
            &mut rand::rng(),
        )
        .expect("XEdDSA challenge signing should succeed");

    // Use a DIFFERENT registration lock than the original. The server treats
    // re-registration with a matching registration_lock as a valid takeover
    // (Phase 0 deletes the old user). With a mismatched lock the server
    // returns PermissionDenied "invalid registration lock", which is the
    // anti-takeover guard this test exercises.
    let dup_registration_lock = [0xAAu8; 32];
    let result = auth_client
        .register_and_wait(
            &dup_username_hash_hex,
            &dup_record,
            &dup_encrypted_name,
            identity_public_key.as_ref(),
            "test-device",
            &random_device_auth_token(),
            &dup_uak,
            Some(prekey_bundle),
            Vec::new(),
            &challenge.challenge.as_ref().expect("challenge token").token,
            &challenge_response,
            common::solve_registration_pow(&challenge),
            &dup_registration_lock,
            &dup_username_lookup_hash,
            &dup_username_proof,
        )
        .await;

    // Should fail because the registration_lock doesn't match the stored hash.
    match result {
        Err(NetworkError::InvalidRequest(msg)) | Err(NetworkError::ServerError(msg)) => {
            assert!(
                msg.contains("registration lock")
                    || msg.contains("registration_lock")
                    || msg.contains("permission")
                    || msg.contains("denied")
                    || msg.contains("invalid")
                    || msg.contains("username")
                    || msg.contains("taken")
                    || msg.contains("exists"),
                "Error should mention re-registration / lock conflict: {msg}"
            );
        }
        Err(NetworkError::PermissionDenied) => {} // Acceptable: server returns PermissionDenied for lock mismatch
        Err(e) => panic!("Expected re-registration conflict error, got: {e:?}"),
        Ok(r) => panic!("Should have failed re-registration without matching lock, got Ok: {r:?}"),
    }
}

// ============================================================================
// Login Tests
// ============================================================================

#[tokio::test]
async fn test_login_success() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let user = assert_ok(register_test_user(&pool).await);

    // OPAQUE login with the same nickname + password, as `AuthService::login`.
    let mut auth_client = AuthClient::new(pool.channel(), pool.auth_clone());
    let response = assert_ok(
        opaque_login(
            &mut auth_client,
            &user.username,
            &user.password,
            "test-device-login",
            &random_svr_public_key(),
            &user.device_auth_token,
            Some(user.device_id),
        )
        .await,
    );

    // AuthResponse contains session and user_profile
    assert!(response.user_profile.is_some());
    let profile = response.user_profile.unwrap();
    assert!(!profile.user_id.is_empty());
    assert_eq!(profile.user_id, user.user_id);
}

#[tokio::test]
async fn test_login_wrong_password() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let user = assert_ok(register_test_user(&pool).await);

    let mut auth_client = AuthClient::new(pool.channel(), pool.auth_clone());
    let result = opaque_login(
        &mut auth_client,
        &user.username,
        "wrong_password",
        "test-device",
        &random_svr_public_key(),
        &user.device_auth_token,
        None,
    )
    .await;

    // The client cannot open KE2 with a wrong password.
    assert_auth_error(result);
}

/// An unknown nickname is answered exactly like a real one: LoginStart
/// succeeds with a full-size KE2 (from the server's fake record), and the
/// login then fails the same way a wrong password does.
#[tokio::test]
async fn test_login_nonexistent_user_is_indistinguishable() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let mut auth_client = AuthClient::new(pool.channel(), pool.auth_clone());

    let start = chatter_core::crypto::opaque::login_start("any password").unwrap();
    let identifier = chatter_core::crypto::auth_crypto::login_identifier(&unique_username());
    let started = assert_ok(auth_client.login_start(&identifier, &start.ke1, None, &[], &[]).await);
    assert_eq!(started.ke2.len(), 320, "fake KE2 has the real KE2's size");
    assert!(matches!(
        chatter_core::crypto::opaque::login_finish_async(start, "any password", started.ke2).await,
        Err(chatter_core::crypto::opaque::OpaqueError::InvalidLogin)
    ));

    let result = opaque_login(
        &mut auth_client,
        &unique_username(),
        "any password",
        "test-device",
        &random_svr_public_key(),
        &random_device_auth_token(),
        None,
    )
    .await;
    assert_auth_error(result);
}

/// Login is priced, not locked (server OPEN-ITEMS §133): after the free
/// attempts the server answers LoginStart with a proof-of-work challenge and
/// no KE2; a bad solution is refused; `login_start_paying_pow` solves, repeats
/// the call, and the owner still logs in with the right password.
#[tokio::test]
async fn test_login_priced_by_proof_of_work_not_locked() {
    use chatter_core::crypto::opaque;
    use chatter_core::services::auth_service::login_start_paying_pow;
    use std::sync::atomic::{AtomicU32, Ordering};
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let user = assert_ok(register_test_user(&pool).await);
    let mut auth_client = AuthClient::new(pool.channel(), pool.auth_clone());
    let identifier = chatter_core::crypto::auth_crypto::login_identifier(&user.username);

    // A stranger's guesses: KE2s until the free tier runs out, then a price.
    let mut challenge = None;
    for _ in 0..10 {
        let ke1 = opaque::login_start("wrong guess").unwrap().ke1;
        let r = assert_ok(auth_client.login_start(&identifier, &ke1, None, &[], &[]).await);
        if let Some(c) = r.pow_challenge {
            assert!(r.ke2.is_empty() && r.login_id.is_empty(), "a challenge carries no KE2");
            challenge = Some(c);
            break;
        }
        assert_eq!(r.ke2.len(), 320);
    }
    let challenge = challenge.expect("repeated failures must price the login");
    assert!(challenge.difficulty >= 6 && challenge.difficulty <= 9, "{}", challenge.difficulty);

    // A solution for the sign-up puzzle over the same token is not a login
    // solution (unless by chance it is both; skip those).
    use chatter_core::crypto::pow::{self, Purpose};
    let reg = (0u64..)
        .find(|s| {
            pow::verify(Purpose::Register, &challenge.token, *s, challenge.difficulty)
                && !pow::verify(Purpose::Login, &challenge.token, *s, challenge.difficulty)
        })
        .unwrap();
    let ke1 = opaque::login_start("wrong guess").unwrap().ke1;
    let refused = auth_client.login_start(&identifier, &ke1, Some((&challenge.token, reg)), &[], &[]).await;
    assert!(
        matches!(&refused, Err(NetworkError::PermissionDenied)),
        "sign-up-domain solution must be refused: {refused:?}"
    );

    // The owner: solved transparently, then a normal login.
    let paid = AtomicU32::new(0);
    let start = opaque::login_start(&user.password).unwrap();
    let started = login_start_paying_pow(&mut auth_client, &identifier, &start.ke1, None, &mut None, &|| {
        paid.fetch_add(1, Ordering::Relaxed);
    })
    .await
    .expect("priced LoginStart must be paid and answered");
    assert_eq!(paid.load(Ordering::Relaxed), 1, "the solve was announced once");
    let done = opaque::login_finish_async(start, &user.password, started.ke2)
        .await
        .expect("right password opens KE2");
    assert_ok(
        auth_client
            .login_finish(&started.login_id, &done.ke3, "d1", &random_svr_public_key(), &random_device_auth_token(), None)
            .await,
    );

    // A verified login wipes the count: the next attempt is free again.
    let ke1 = opaque::login_start(&user.password).unwrap().ke1;
    let r = assert_ok(auth_client.login_start(&identifier, &ke1, None, &[], &[]).await);
    assert!(r.pow_challenge.is_none());
}

/// The login guess ceiling (server OPEN-ITEMS §133): paid guesses are
/// hard-capped per account and the client gets `AuthError::LoginGuessLimit`
/// with text pointing at the ways in. Those ways in work while it is full: a
/// signed-in device still proves the password, and a recovery code needs no
/// password and returns a grant that gets the new password through a ceiling
/// someone burnt again right after.
#[tokio::test]
async fn test_login_guess_ceiling_and_its_bypasses() {
    use chatter_core::crypto::opaque;
    use chatter_core::services::auth_service::{login_start_paying_pow, AuthError};
    skip_if_no_server!();

    // A signed-in device with fresh recovery codes.
    let device_pool = assert_ok(create_test_pool().await);
    let user = assert_ok(setup_authenticated_test(&device_pool).await);
    let mut device = AuthClient::new(device_pool.channel(), device_pool.auth_clone());
    let prove = |pw: String| {
        let mut device = AuthClient::new(device_pool.channel(), device_pool.auth_clone());
        async move {
            let start = opaque::login_start(&pw).unwrap();
            let opened = device.password_proof_start(&start.ke1).await?;
            let done = opaque::login_finish_async(start, &pw, opened.ke2).await.unwrap();
            Ok::<_, NetworkError>(chatter_core::proto::chat_v1::PasswordProof {
                proof_id: opened.proof_id,
                ke3: done.ke3,
            })
        }
    };
    let codes = assert_ok(device.regenerate_recovery_codes(assert_ok(prove(user.password.clone()).await)).await);

    // A stranger burns the account's guesses, paying for each.
    let pool = assert_ok(create_test_pool().await);
    let mut stranger = AuthClient::new(pool.channel(), pool.auth_clone());
    let identifier = chatter_core::crypto::auth_crypto::login_identifier(&user.username);
    let burn = || {
        let mut stranger = AuthClient::new(pool.channel(), pool.auth_clone());
        let identifier = identifier.clone();
        async move {
            for _ in 0..30 {
                let ke1 = opaque::login_start("wrong guess").unwrap().ke1;
                match login_start_paying_pow(&mut stranger, &identifier, &ke1, None, &mut None, &|| {}).await {
                    Ok(_) => continue,
                    Err(e) => return e,
                }
            }
            panic!("30 paid guesses and no ceiling");
        }
    };
    let err = burn().await;
    assert!(matches!(err, AuthError::LoginGuessLimit), "{err:?}");
    let text = err.user_message();
    assert!(text.contains("recovery code") && text.contains("link"), "{text}");

    // The owner's password, too, gets no KE2 now...
    let ke1 = opaque::login_start(&user.password).unwrap().ke1;
    let refused = login_start_paying_pow(&mut stranger, &identifier, &ke1, None, &mut None, &|| {}).await;
    assert!(matches!(refused, Err(AuthError::LoginGuessLimit)), "{refused:?}");
    // ...but the signed-in device still proves it (per-device budget).
    assert_ok(device.verify_password(assert_ok(prove(user.password.clone()).await)).await);

    // Recovery: no password, not gated, returns a grant.
    let new_pw = "a brand new passphrase 77";
    let reg = opaque::registration_start(new_pw).unwrap();
    let started = assert_ok(stranger.use_recovery_code_start(&codes[0], &reg.request).await);
    let record = opaque::registration_finish_async(reg, new_pw, started.registration_response)
        .await
        .unwrap();
    let finished = assert_ok(
        stranger
            .use_recovery_code_finish(&started.recovery_id, &record, &random_svr_public_key(), &[7u8; 32])
            .await,
    );
    assert_eq!(finished.login_grant.len(), 32);

    // Burnt again straight after; the grant still gets the owner in.
    assert!(matches!(burn().await, AuthError::LoginGuessLimit));
    let start = opaque::login_start(new_pw).unwrap();
    let started = login_start_paying_pow(&mut stranger, &identifier, &start.ke1, Some(&finished.login_grant), &mut None, &|| {})
        .await
        .expect("the recovery grant skips the ceiling");
    let done = opaque::login_finish_async(start, new_pw, started.ke2).await.expect("new password opens KE2");
    assert_ok(
        stranger
            .login_finish(&started.login_id, &done.ke3, "d1", &random_svr_public_key(), &random_device_auth_token(), None)
            .await,
    );
}

/// Trusted-device tokens (server OPEN-ITEMS §134): a device that logged in
/// before keeps getting in while a stranger holds the account's shared guess
/// ceiling full; a token the server no longer honours is dropped and the
/// login goes on without it. Needs a dev server running this change.
#[tokio::test]
async fn test_trusted_device_token_outlasts_burnt_ceiling() {
    use chatter_core::crypto::opaque;
    use chatter_core::services::auth_service::{login_start_paying_pow, AuthError};
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let user = assert_ok(register_test_user(&pool).await);
    let mut device = AuthClient::new(pool.channel(), pool.auth_clone());
    let identifier = chatter_core::crypto::auth_crypto::login_identifier(&user.username);

    // A first password login on "this device" hands out a token.
    let login = |token: Option<Vec<u8>>| {
        let identifier = identifier.clone();
        let pw = user.password.clone();
        let mut device = AuthClient::new(pool.channel(), pool.auth_clone());
        async move {
            let mut token = token;
            let start = opaque::login_start(&pw).unwrap();
            let started = login_start_paying_pow(&mut device, &identifier, &start.ke1, None, &mut token, &|| {}).await?;
            let done = opaque::login_finish_async(start, &pw, started.ke2).await.unwrap();
            let resp = device
                .login_finish(&started.login_id, &done.ke3, "d1", &random_svr_public_key(), &random_device_auth_token(), None)
                .await
                .map_err(AuthError::Network)?;
            Ok::<_, AuthError>((token, resp.trusted_device_token))
        }
    };
    let (_, token) = login(None).await.expect("first login");
    assert_eq!(token.len(), 57, "LoginFinish issues a trusted-device token");

    // A stranger fills the shared ceiling, paying for each guess.
    let mut stranger = AuthClient::new(pool.channel(), pool.auth_clone());
    let mut burnt = false;
    for _ in 0..30 {
        let ke1 = opaque::login_start("wrong guess").unwrap().ke1;
        if let Err(e) = login_start_paying_pow(&mut stranger, &identifier, &ke1, None, &mut None, &|| {}).await {
            assert!(matches!(e, AuthError::LoginGuessLimit), "{e:?}");
            burnt = true;
            break;
        }
    }
    assert!(burnt, "30 paid guesses and no ceiling");
    assert!(matches!(login(None).await, Err(AuthError::LoginGuessLimit)), "no token: locked out");

    // With the token the device still gets in, and receives a fresh one.
    let (kept, fresh) = login(Some(token.clone())).await.expect("token bypasses the shared ceiling");
    assert_eq!(kept.as_deref(), Some(token.as_slice()), "a valid token is kept");
    assert_eq!(fresh.len(), 57);

    // A token the server did not issue is dropped (and, the ceiling being
    // full, the login then fails like any token-less one).
    let mut forged = fresh.clone();
    forged[56] ^= 1;
    let mut dropped = Some(forged);
    let ke1 = opaque::login_start(&user.password).unwrap().ke1;
    let r = login_start_paying_pow(&mut device, &identifier, &ke1, None, &mut dropped, &|| {}).await;
    assert!(dropped.is_none(), "an invalid token is dropped");
    assert!(matches!(r, Err(AuthError::LoginGuessLimit)), "{r:?}");
}

/// A LoginFinish is single-use: replaying the same login_id + KE3 fails.
#[tokio::test]
async fn test_login_finish_replay_rejected() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let user = assert_ok(register_test_user(&pool).await);
    let mut auth_client = AuthClient::new(pool.channel(), pool.auth_clone());

    let start = chatter_core::crypto::opaque::login_start(&user.password).unwrap();
    let identifier = chatter_core::crypto::auth_crypto::login_identifier(&user.username);
    let started = assert_ok(auth_client.login_start(&identifier, &start.ke1, None, &[], &[]).await);
    let done = chatter_core::crypto::opaque::login_finish_async(start, &user.password, started.ke2)
        .await
        .expect("right password opens KE2");
    let svr = random_svr_public_key();
    assert_ok(
        auth_client
            .login_finish(&started.login_id, &done.ke3, "d1", &svr, &random_device_auth_token(), None)
            .await,
    );
    let replay = auth_client
        .login_finish(&started.login_id, &done.ke3, "d2", &svr, &random_device_auth_token(), None)
        .await;
    assert_auth_error(replay);
}

// ============================================================================
// Logout Tests
// ============================================================================

#[tokio::test]
async fn test_logout() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let user = assert_ok(setup_authenticated_test(&pool).await);

    let mut auth_client = AuthClient::new(pool.channel(), pool.auth_clone());
    let result = auth_client.logout(user.device_id).await;

    assert!(result.is_ok(), "Logout should succeed");
}

// ============================================================================
// Device Management Tests
// ============================================================================

#[tokio::test]
async fn test_list_devices() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let user = assert_ok(setup_authenticated_test(&pool).await);

    let mut auth_client = AuthClient::new(pool.channel(), pool.auth_clone());
    let response = assert_ok(auth_client.list_devices().await);

    assert!(!response.devices.is_empty(), "Should have at least one device");

    // Find our device
    let our_device = response
        .devices
        .iter()
        .find(|d| d.device_id == user.device_id);
    assert!(our_device.is_some(), "Should find our device in list");
}

#[tokio::test]
async fn test_list_sessions() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let _user = assert_ok(setup_authenticated_test(&pool).await);

    let mut auth_client = AuthClient::new(pool.channel(), pool.auth_clone());
    let response = assert_ok(auth_client.list_sessions().await);

    // Should have at least one active session
    assert!(
        !response.sessions.is_empty(),
        "Should have at least one active session"
    );
}

// ============================================================================
// Trust Root Tests (Hardcoded - Signal Security Model)
// ============================================================================

#[tokio::test]
async fn test_hardcoded_trust_root() {
    // Trust root is baked into the binary at compile time following Signal's
    // security model. This prevents MITM attacks during registration.
    //
    // `None` means this build has no trust root configured — legitimate (the
    // binary panics loudly on first use, see config.rs), so there is nothing to
    // assert about the bytes.
    use chatter_core::config::TRUST_ROOT_PUBLIC_KEY_BYTES;

    if let Some(bytes) = TRUST_ROOT_PUBLIC_KEY_BYTES {
        assert_eq!(bytes.len(), 32, "Trust root must be 32 bytes");
        assert_ne!(bytes, [0u8; 32], "Trust root must not be all zeros");
    }
}

// ============================================================================
// Sender Certificate Tests
// ============================================================================

#[tokio::test]
async fn test_request_sender_certificate() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let user = assert_ok(setup_authenticated_test(&pool).await);

    let mut auth_client = AuthClient::new(pool.channel(), pool.auth_clone());
    let _ = user.device_id; // server reads device_id from auth session
    let response = assert_ok(
        auth_client
            .request_sender_certificate()
            .await,
    );

    assert!(
        response.certificate.is_some(),
        "Should return a sender certificate"
    );
    let cert = response.certificate.unwrap();
    assert!(!cert.certificate.is_empty(), "Certificate data should not be empty");
}

// ============================================================================
// Password Change Tests
// ============================================================================

#[tokio::test]
async fn test_change_password() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let user = assert_ok(setup_authenticated_test(&pool).await);
    let new_password = "new_secure_password";
    let mut auth_client = AuthClient::new(pool.channel(), pool.auth_clone());

    // OPAQUE: KE1 for the current password + RegistrationRequest for the new,
    // then KE3 + RegistrationRecord.
    let proof = chatter_core::crypto::opaque::login_start(&user.password).unwrap();
    let reg = chatter_core::crypto::opaque::registration_start(new_password).unwrap();
    let started = assert_ok(auth_client.change_password_start(&proof.ke1, &reg.request).await);
    let done = chatter_core::crypto::opaque::login_finish_async(proof, &user.password, started.ke2)
        .await
        .expect("current password opens KE2");
    let record = chatter_core::crypto::opaque::registration_finish_async(
        reg,
        new_password,
        started.registration_response,
    )
    .await
    .unwrap();
    assert_ok(auth_client.change_password_finish(&started.flow_id, &done.ke3, &record).await);

    // The account must now be reachable with the new password only, under
    // the SAME login identifier (it is nickname-derived).
    let old = opaque_login(
        &mut auth_client,
        &user.username,
        &user.password,
        "test-device-old-password",
        &random_svr_public_key(),
        &user.device_auth_token,
        Some(user.device_id),
    )
    .await;
    assert_auth_error(old);
    let response = assert_ok(
        opaque_login(
            &mut auth_client,
            &user.username,
            new_password,
            "test-device-after-password-change",
            &random_svr_public_key(),
            &user.device_auth_token,
            Some(user.device_id),
        )
        .await,
    );
    let profile = response.user_profile.expect("login returns the profile");
    assert_eq!(profile.user_id, user.user_id);
}

// ============================================================================
// Protected Endpoint Tests
// ============================================================================

#[tokio::test]
async fn test_unauthenticated_protected_endpoint() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);

    // Don't set up authentication
    let mut auth_client = AuthClient::new(pool.channel(), pool.auth_clone());
    let result = auth_client.list_devices().await;

    assert_unauthenticated(result);
}

// ============================================================================
// Prekey Tests
// ============================================================================

#[tokio::test]
async fn test_get_prekey_stats() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let user = assert_ok(setup_authenticated_test(&pool).await);

    let mut auth_client = AuthClient::new(pool.channel(), pool.auth_clone());
    let response = assert_ok(auth_client.get_pre_key_stats(user.device_id).await);

    // New user may have 0 prekeys (we didn't upload any)
    assert_eq!(response.device_id, user.device_id);
}

#[tokio::test]
async fn test_upload_and_get_prekey_bundle() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let user = assert_ok(setup_authenticated_test(&pool).await);

    // Create a prekey bundle with real Signal keys
    use chatter_core::proto::chat_v1::signal_pre_key_bundle::{PreKeyRecord, SignedPreKeyRecord};
    use chatter_core::proto::chat_v1::SignalPreKeyBundle;
    use libsignal_protocol::{IdentityKeyPair, KeyPair};

    // Reuse the identity key pair from registration. The server enforces
    // "all devices for a user share the same identity key" (see
    // signal_protocol.go::StoreSignalPreKeyBundle), so generating a fresh
    // pair here would be rejected with an identity-key-mismatch error.
    let identity_key_pair = IdentityKeyPair::try_from(user.identity_key_pair_bytes.as_slice())
        .expect("registered identity key pair should deserialize");
    let identity_public_key = identity_key_pair.identity_key().serialize();

    // Generate signed prekey with valid signature
    let signed_prekey_pair = KeyPair::generate(&mut rand::rng());
    let signed_prekey_signature = identity_key_pair
        .private_key()
        .calculate_signature(&signed_prekey_pair.public_key.serialize(), &mut rand::rng())
        .expect("Signature should succeed");

    // Generate one-time prekey
    let one_time_prekey_pair = KeyPair::generate(&mut rand::rng());

    let bundle = SignalPreKeyBundle {
        identity_key: identity_public_key.as_ref().to_vec(),
        registration_id: 12345,
        device_id: user.device_id,
        signed_prekey: Some(SignedPreKeyRecord {
            id: 2, // Different from registration bundle
            public_key: signed_prekey_pair.public_key.serialize().to_vec(),
            signature: signed_prekey_signature.into_vec(),
            // Server's UploadPreKeyBundle calls ValidatePreKeyTimestamps which
            // interprets this as Unix seconds; ms would be far in the future.
            timestamp: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs(),
        }),
        one_time_prekey: Some(PreKeyRecord {
            id: 100,
            public_key: one_time_prekey_pair.public_key.serialize().to_vec(),
        }),
        kyber_prekey: None,
        unrestricted_unidentified_access: Some(false),
    };

    let mut auth_client = AuthClient::new(pool.channel(), pool.auth_clone());

    // Upload the bundle
    let upload_result = auth_client.upload_pre_key_bundle(bundle).await;
    assert!(upload_result.is_ok(), "Should be able to upload prekey bundle: {:?}", upload_result.err());

    // Fetch it back (as another user would)
    let get_result = auth_client
        .get_pre_key_bundle(&user.user_id, &user.device_id.to_string())
        .await;

    // Note: Fetching own prekeys may succeed or fail depending on server implementation
    // The important thing is the upload succeeded
    match get_result {
        Ok(response) => {
            // GetPreKeyBundleResponse uses a oneof result
            assert!(response.result.is_some(), "Should return a bundle");
        }
        Err(NetworkError::NotFound(_)) => {
            // Some servers don't allow fetching own prekeys
        }
        Err(e) => panic!("Unexpected error: {:?}", e),
    }
}

//! Test fixtures for creating test data.
//!
//! Provides utilities for generating unique test usernames,
//! passwords, keys, and registering test users.

use std::sync::Arc;

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use rand::RngCore;

use chatter_core::error::NetworkError;
use chatter_core::network::grpc::{AuthClient, GrpcClientPool};
use chatter_core::proto::chat_v1::signal_pre_key_bundle::{
    KyberPreKeyRecord as ProtoKyberPreKey, PreKeyRecord, SignedPreKeyRecord,
};
use chatter_core::proto::chat_v1::{AuthResponse, SignalPreKeyBundle};
use libsignal_protocol::{kem, IdentityKeyPair, KeyPair, KyberPreKeyId, KyberPreKeyRecord, GenericSignedPreKey};

/// Test user data returned after registration.
#[derive(Debug, Clone)]
pub struct TestUser {
    /// Unique base nickname (no discriminator). Pre-Signal grammar.
    pub username: String,
    /// Full Signal-grammar username `nickname.discriminator` — the value
    /// the Ristretto255 lookup hash was computed over and what
    /// `LookupUsername` resolves on the server.
    pub full_username: String,
    /// Display name (plaintext).
    pub display_name: String,
    /// Encrypted display name (nonce || ciphertext || auth_tag).
    pub encrypted_display_name: Vec<u8>,
    /// Plain password (for re-login tests). The server only ever saw an
    /// OPAQUE registration record for it.
    pub password: String,
    /// 32-byte Ed25519 public key.
    pub public_key: Vec<u8>,
    /// Serialized identity key pair (private + public) for tests that need to
    /// upload follow-on prekey bundles signed by the same identity. The
    /// server enforces "all devices share the same identity key" — if a test
    /// re-generates a fresh `IdentityKeyPair`, the upload is rejected.
    pub identity_key_pair_bytes: Vec<u8>,
    /// 32-byte device auth token.
    pub device_auth_token: Vec<u8>,
    /// 32-byte profile key.
    pub profile_key: [u8; 32],
    /// User ID returned from server.
    pub user_id: String,
    /// Device ID returned from server.
    pub device_id: u32,
}

impl TestUser {
    /// Encrypt the display name with the user's profile key.
    ///
    /// Returns raw encrypted bytes (nonce || ciphertext || auth_tag).
    pub fn encrypt_display_name(&self) -> Vec<u8> {
        chatter_core::crypto::profile_crypto::encrypt_display_name(
            &self.profile_key,
            &self.display_name,
        )
        .expect("Display name encryption should succeed")
    }

    /// Decrypt an encrypted display name with the user's profile key.
    pub fn decrypt_display_name(&self, encrypted: &[u8]) -> Option<String> {
        chatter_core::crypto::profile_crypto::decrypt_display_name(
            &self.profile_key,
            encrypted,
        )
        .ok()
    }

    /// Decrypt a base64-encoded encrypted display name.
    pub fn decrypt_display_name_base64(&self, encrypted_base64: &str) -> Option<String> {
        let encrypted = BASE64.decode(encrypted_base64).ok()?;
        self.decrypt_display_name(&encrypted)
    }

    /// Derive Unidentified Access Key from profile key.
    pub fn derive_uak(&self) -> [u8; 16] {
        chatter_core::crypto::derive_uak(&self.profile_key)
    }
}

/// Generate a unique username for testing.
///
/// Format: `test_user_{random_hex}_{timestamp}`
pub fn unique_username() -> String {
    let mut random_bytes = [0u8; 4];
    rand::rng().fill_bytes(&mut random_bytes);
    let random_hex = hex::encode(random_bytes);
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis();
    format!("test_user_{}_{}", random_hex, timestamp % 100000)
}

/// Generate a random password.
pub fn random_password() -> String {
    let mut random_bytes = [0u8; 16];
    rand::rng().fill_bytes(&mut random_bytes);
    format!("TestPass_{}", hex::encode(random_bytes))
}

/// OPAQUE login through a raw [`AuthClient`] — exactly what
/// `AuthService::login` sends: `LoginStart` with the nickname-derived
/// identifier and KE1, then `LoginFinish` with KE3 and the device fields.
///
/// A wrong password never reaches `LoginFinish`: the client cannot open KE2,
/// which surfaces here as [`NetworkError::Unauthenticated`] so callers can
/// treat it like the server's rejection.
pub async fn opaque_login(
    auth_client: &mut AuthClient,
    username: &str,
    password: &str,
    device_info: &str,
    svr_public_key: &[u8],
    device_auth_token: &[u8],
    device_id: Option<u32>,
) -> Result<AuthResponse, NetworkError> {
    use chatter_core::crypto::opaque;
    let identifier = chatter_core::crypto::auth_crypto::login_identifier(username);
    let start = opaque::login_start(password).map_err(|e| NetworkError::Unknown(e.to_string()))?;
    // Pays the server's login proof of work when it asks one, as the app does.
    let started = chatter_core::services::auth_service::login_start_paying_pow(
        auth_client,
        &identifier,
        &start.ke1,
        None,
        &|| {},
    )
    .await
    .map_err(|e| match e {
        chatter_core::services::auth_service::AuthError::Network(n) => n,
        other => NetworkError::Unknown(other.to_string()),
    })?;
    let done = match opaque::login_finish_async(start, password, started.ke2).await {
        Ok(d) => d,
        Err(opaque::OpaqueError::InvalidLogin) => return Err(NetworkError::Unauthenticated),
        Err(e) => return Err(NetworkError::Unknown(e.to_string())),
    };
    auth_client
        .login_finish(&started.login_id, &done.ke3, device_info, svr_public_key, device_auth_token, device_id)
        .await
}

/// An X25519 SVR public key from a random secret (what a PIN would derive).
pub fn random_svr_public_key() -> Vec<u8> {
    let mut secret = [0u8; 32];
    rand::rng().fill_bytes(&mut secret);
    chatter_core::crypto::svr::SvrKeypair::from_secret(&secret).public_key_bytes().to_vec()
}

/// Generate a random 32-byte Ed25519-like public key.
pub fn random_public_key() -> Vec<u8> {
    let mut key = vec![0u8; 32];
    rand::rng().fill_bytes(&mut key);
    key
}

/// Generate a random 33-byte Signal identity key (0x05 prefix + 32 bytes).
pub fn random_identity_key() -> Vec<u8> {
    let mut key = vec![0x05u8]; // Signal DJB key type prefix
    let mut data = vec![0u8; 32];
    rand::rng().fill_bytes(&mut data);
    key.extend(data);
    key
}

/// Generate a random 32-byte device auth token.
pub fn random_device_auth_token() -> Vec<u8> {
    let mut token = vec![0u8; 32];
    rand::rng().fill_bytes(&mut token);
    token
}

/// Generate a random 32-byte profile key.
pub fn random_profile_key() -> [u8; 32] {
    let mut key = [0u8; 32];
    rand::rng().fill_bytes(&mut key);
    key
}

/// Generate a random 16-byte Unidentified Access Key.
pub fn random_uak() -> [u8; 16] {
    let mut uak = [0u8; 16];
    rand::rng().fill_bytes(&mut uak);
    uak
}

/// Create a connected GrpcClientPool for testing.
pub async fn create_test_pool() -> Result<Arc<GrpcClientPool>, NetworkError> {
    // The pool-based proofs never went through the ChatterClient builder, which
    // is where validated.rs installs its subscriber — so `RUST_LOG` did nothing
    // for exactly the tests whose failures were hardest to read (OPEN-ITEMS
    // §73). No-op unless RUST_LOG is set; try_init tolerates repeats.
    if std::env::var("RUST_LOG").is_ok() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .try_init();
    }

    let pool = GrpcClientPool::new(&super::server_url())?;
    pool.connect().await?;
    Ok(Arc::new(pool))
}

/// Create a connected GrpcClientPool for streaming tests (no request timeout).
pub async fn create_streaming_test_pool() -> Result<Arc<GrpcClientPool>, NetworkError> {
    let pool = GrpcClientPool::new_for_streaming(&super::server_url())?;
    pool.connect().await?;
    Ok(Arc::new(pool))
}

/// Register a new test user and return their data.
///
/// Uses the streaming registration API and waits for completion.
/// Includes a real Signal Protocol prekey bundle for sealed sender support.
pub async fn register_test_user(pool: &GrpcClientPool) -> Result<TestUser, NetworkError> {
    let username = unique_username();
    // Build a Signal-grammar `nickname.discriminator` — required so the
    // server's `LookupUsername` RPC (which matches against the Ristretto
    // lookup hash) can resolve us back. Without this the integration test
    // `test_lookup_username_exact_match` returns None.
    let full_username = chatter_core::crypto::auth_crypto::username_candidates(&username)
        .map_err(|e| NetworkError::Unknown(format!("invalid test nickname: {e}")))?
        .into_iter()
        .next()
        .ok_or_else(|| NetworkError::Unknown("no username candidates".to_string()))?;
    let display_name = format!("Test User {}", &username[10..18]);
    let password = random_password();
    let device_auth_token = random_device_auth_token();
    let profile_key = random_profile_key();

    // Generate real Signal Protocol identity key pair
    let identity_key_pair = IdentityKeyPair::generate(&mut rand::rng());
    let identity_public_key = identity_key_pair.identity_key().serialize();

    // Generate real signed prekey with valid signature
    let signed_prekey_pair = KeyPair::generate(&mut rand::rng());
    let signed_prekey_signature = identity_key_pair
        .private_key()
        .calculate_signature(&signed_prekey_pair.public_key.serialize(), &mut rand::rng())
        .expect("Signature generation should succeed");
    // NANOSECONDS: the server reads this field with time.Unix(0, ts). Sending
    // millis dated every fixture bundle to 1970-01-01T00:29:44Z, which the
    // registration workflow's prekey-age validation rejects outright ("signed
    // prekey too old: age 495605h exceeds max 168h"), so every fixture-based
    // registration against a current server failed before it began.
    let signed_prekey_timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64;

    // Generate one-time prekey
    let one_time_prekey_pair = KeyPair::generate(&mut rand::rng());

    // Generate Kyber prekey with valid signature
    let kyber_prekey = KyberPreKeyRecord::generate(
        kem::KeyType::Kyber1024,
        KyberPreKeyId::from(1u32),
        identity_key_pair.private_key(),
    )
    .expect("Kyber prekey generation should succeed");

    let registration_id = rand::random::<u32>() % 16380 + 1;

    // Create prekey bundle with real keys and signatures
    let prekey_bundle = SignalPreKeyBundle {
        identity_key: identity_public_key.as_ref().to_vec(),
        registration_id,
        device_id: 1, // Primary device
        signed_prekey: Some(SignedPreKeyRecord {
            id: 1,
            public_key: signed_prekey_pair.public_key.serialize().to_vec(),
            signature: signed_prekey_signature.into_vec(),
            timestamp: signed_prekey_timestamp,
        }),
        one_time_prekey: Some(PreKeyRecord {
            id: 1,
            public_key: one_time_prekey_pair.public_key.serialize().to_vec(),
        }),
        kyber_prekey: Some(ProtoKyberPreKey {
            id: 1,
            public_key: kyber_prekey.public_key().expect("key").serialize().to_vec(),
            signature: kyber_prekey.signature().expect("sig"),
            timestamp: signed_prekey_timestamp,
        }),
        unrestricted_unidentified_access: Some(false),
    };

    let mut auth_client = AuthClient::new(pool.channel(), pool.auth_clone());

    // Encrypt display name with profile key (required for Phase 79 privacy)
    let encrypted_display_name = chatter_core::crypto::profile_crypto::encrypt_display_name(
        &profile_key,
        &display_name,
    )
    .expect("Display name encryption should succeed");

    // Generate unidentified access key (16 bytes) for sealed sender
    let uak = chatter_core::crypto::derive_uak(&profile_key);

    // Server now requires a registration challenge proving identity-key
    // possession. Mirror what `AuthService::register` does in the SDK:
    // the nickname-derived login identifier, an OPAQUE RegistrationRequest
    // riding the challenge, the domain-separated challenge message
    // (registration_challenge_message) signed with our identity key (XEdDSA),
    // and the finalized OPAQUE record uploaded with Register.
    let username_hash_hex = chatter_core::crypto::auth_crypto::login_identifier(&username);
    // SDK strips the leading 0x05 type byte before sending; match that.
    let identity_public_key_stripped = &identity_public_key[1..];
    let opaque_reg = chatter_core::crypto::opaque::registration_start(&password)
        .map_err(|e| NetworkError::Unknown(e.to_string()))?;
    let challenge = auth_client
        .request_registration_challenge(
            &username_hash_hex,
            identity_public_key_stripped,
            &opaque_reg.request,
        )
        .await?;
    let opaque_record = chatter_core::crypto::opaque::registration_finish_async(
        opaque_reg,
        &password,
        challenge.opaque_registration_response.clone(),
    )
    .await
    .map_err(|e| NetworkError::Unknown(e.to_string()))?;
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

    // Ristretto255 lookup hash over the full `name.discriminator` —
    // server's LookupUsername RPC matches against THIS column.
    // The server verifies the accompanying ZK proof before storing the hash.
    let (username_lookup, username_proof) =
        chatter_core::crypto::auth_crypto::username_claim(&full_username)
            .map_err(|e| NetworkError::Unknown(format!("username_claim failed: {e}")))?;

    // The SVR public key is an X25519 key derived from the account PIN — 32 raw
    // bytes, no type prefix (`crypto/svr.rs:394`). This fixture used to pass
    // `identity_public_key.as_ref()` here: the Signal IDENTITY key, 33 bytes of
    // `0x05 || u`, in the argument named svr_public_key. Every test user was
    // registered with its identity key stored in `users.svr_public_key` — a
    // column with a UNIQUE constraint, used for account recovery. It went
    // unnoticed because the server's validation only checked a length RANGE
    // (>= 32, <= 128); tightening it to a real X25519 check (CRYPTO-8,
    // OPEN-ITEMS §84) is what surfaced it.
    let mut svr_secret = [0u8; 32];
    rand::rng().fill_bytes(&mut svr_secret);
    let svr_public_key =
        chatter_core::crypto::svr::SvrKeypair::from_secret(&svr_secret).public_key_bytes();

    let result = auth_client
        .register_and_wait(
            &username_hash_hex,        // PRIVACY: Send hash, matches SDK (server stores hash, not plaintext)
            &opaque_record,
            &encrypted_display_name,
            &svr_public_key,
            "integration-test-device",
            &device_auth_token,
            &uak,
            Some(prekey_bundle),
            Vec::new(), // No additional one-time prekeys for tests
            &challenge.challenge.as_ref().expect("challenge token").token,
            &challenge_response,
            solve_registration_pow(&challenge),
            &[0u8; 32],         // Empty registration lock for tests
            &username_lookup,   // username_claim(full_username): discovery hash
            &username_proof,    // ...and the proof the server verifies it against
        )
        .await?;

    Ok(TestUser {
        username,
        full_username,
        display_name,
        encrypted_display_name,
        password,
        public_key: identity_public_key.as_ref().to_vec(),
        identity_key_pair_bytes: identity_key_pair.serialize().into_vec(),
        device_auth_token,
        profile_key,
        user_id: result.user_id,
        device_id: result.device_id,
    })
}

/// Register a test user and configure the pool for authenticated requests.
///
/// Sets up Basic auth in Signal style: `{user_id}.{device_id}:{token}`.
pub async fn setup_authenticated_test(
    pool: &GrpcClientPool,
) -> Result<TestUser, NetworkError> {
    let user = register_test_user(pool).await?;

    // Configure auth interceptor with Signal-style Basic auth
    let username = format!("{}.{}", user.user_id, user.device_id);
    let token = BASE64.encode(&user.device_auth_token);
    pool.auth().set_basic(username, token);

    Ok(user)
}

/// Encrypt a display name for testing.
///
/// Uses the provided profile key to encrypt the display name.
/// Returns raw encrypted bytes (nonce || ciphertext || auth_tag).
pub fn encrypt_test_display_name(profile_key: &[u8; 32], display_name: &str) -> Vec<u8> {
    chatter_core::crypto::profile_crypto::encrypt_display_name(profile_key, display_name)
        .expect("Display name encryption should succeed")
}

/// Encrypt a display name for testing and return as base64.
pub fn encrypt_test_display_name_base64(profile_key: &[u8; 32], display_name: &str) -> String {
    BASE64.encode(&encrypt_test_display_name(profile_key, display_name))
}

/// Create a fake encrypted display name (valid format, random content).
///
/// Returns raw bytes (156 bytes = 12 nonce + 128 padded + 16 tag).
/// Useful for tests that need valid-looking encrypted display names.
pub fn fake_encrypted_display_name() -> Vec<u8> {
    let mut fake_ciphertext = vec![0u8; 156];
    rand::rng().fill_bytes(&mut fake_ciphertext);
    fake_ciphertext
}

/// Create a fake encrypted display name as base64 string.
pub fn fake_encrypted_display_name_base64() -> String {
    BASE64.encode(&fake_encrypted_display_name())
}

/// Generate a valid Signal Protocol prekey bundle with real cryptographic signatures.
/// Returns (prekey_bundle, identity_public_key) tuple.
pub fn generate_prekey_bundle() -> (SignalPreKeyBundle, Vec<u8>) {
    // Generate real identity keypair
    let identity_key_pair = IdentityKeyPair::generate(&mut rand::rng());
    let identity_public_key = identity_key_pair.identity_key().serialize();

    // Generate real signed prekey with valid signature
    let signed_prekey_pair = KeyPair::generate(&mut rand::rng());
    let signed_prekey_signature = identity_key_pair
        .private_key()
        .calculate_signature(&signed_prekey_pair.public_key.serialize(), &mut rand::rng())
        .expect("Signature generation should succeed");
    // NANOSECONDS: the server reads this field with time.Unix(0, ts). Sending
    // millis dated every fixture bundle to 1970-01-01T00:29:44Z, which the
    // registration workflow's prekey-age validation rejects outright ("signed
    // prekey too old: age 495605h exceeds max 168h"), so every fixture-based
    // registration against a current server failed before it began.
    let signed_prekey_timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64;

    // Generate one-time prekey
    let one_time_prekey_pair = KeyPair::generate(&mut rand::rng());

    // Generate Kyber prekey with valid signature
    let kyber_prekey = KyberPreKeyRecord::generate(
        kem::KeyType::Kyber1024,
        KyberPreKeyId::from(1u32),
        identity_key_pair.private_key(),
    )
    .expect("Kyber prekey generation should succeed");

    let registration_id = rand::random::<u32>() % 16380 + 1;

    let prekey_bundle = SignalPreKeyBundle {
        identity_key: identity_public_key.as_ref().to_vec(),
        registration_id,
        device_id: 1,
        signed_prekey: Some(SignedPreKeyRecord {
            id: 1,
            public_key: signed_prekey_pair.public_key.serialize().to_vec(),
            signature: signed_prekey_signature.into_vec(),
            timestamp: signed_prekey_timestamp,
        }),
        one_time_prekey: Some(PreKeyRecord {
            id: 1,
            public_key: one_time_prekey_pair.public_key.serialize().to_vec(),
        }),
        kyber_prekey: Some(ProtoKyberPreKey {
            id: 1,
            public_key: kyber_prekey.public_key().expect("key").serialize().to_vec(),
            signature: kyber_prekey.signature().expect("sig"),
            timestamp: signed_prekey_timestamp,
        }),
        unrestricted_unidentified_access: Some(false),
    };

    (prekey_bundle, identity_public_key.as_ref().to_vec())
}

/// Pays a registration challenge's proof of work (whatever the transport's
/// difficulty; 0 on clearnet).
pub fn solve_registration_pow(challenge: &chatter_core::proto::chat_v1::RequestRegistrationChallengeResponse) -> u64 {
    let c = challenge.challenge.as_ref().expect("challenge token");
    chatter_core::crypto::pow::solve(
        chatter_core::crypto::pow::Purpose::Register,
        &c.token,
        c.difficulty,
        std::time::Instant::now() + std::time::Duration::from_secs(240),
    )
    .expect("registration proof of work")
}

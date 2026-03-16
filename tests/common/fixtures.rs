//! Test fixtures for creating test data.
//!
//! Provides utilities for generating unique test usernames,
//! passwords, keys, and registering test users.

use std::sync::Arc;

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use rand::RngCore;
use sha2::{Digest, Sha256};

use chatter_linux::error::NetworkError;
use chatter_linux::network::grpc::{AuthClient, GrpcClientPool};
use chatter_linux::proto::chat_v1::signal_pre_key_bundle::{
    KyberPreKeyRecord as ProtoKyberPreKey, PreKeyRecord, SignedPreKeyRecord,
};
use chatter_linux::proto::chat_v1::SignalPreKeyBundle;
use libsignal_protocol::{kem, IdentityKeyPair, KeyPair, KyberPreKeyId, KyberPreKeyRecord, GenericSignedPreKey};

/// Test user data returned after registration.
#[derive(Debug, Clone)]
pub struct TestUser {
    /// Unique username.
    pub username: String,
    /// Display name (plaintext).
    pub display_name: String,
    /// Encrypted display name (nonce || ciphertext || auth_tag).
    pub encrypted_display_name: Vec<u8>,
    /// SHA-256 hashed password.
    pub password_hash: Vec<u8>,
    /// Plain password (for re-login tests).
    pub password: String,
    /// 32-byte Ed25519 public key.
    pub public_key: Vec<u8>,
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
        chatter_linux::crypto::profile_crypto::encrypt_display_name(
            &self.profile_key,
            &self.display_name,
        )
        .expect("Display name encryption should succeed")
    }

    /// Get encrypted display name as base64 string (for API calls).
    pub fn encrypted_display_name_base64(&self) -> String {
        BASE64.encode(&self.encrypted_display_name)
    }

    /// Decrypt an encrypted display name with the user's profile key.
    pub fn decrypt_display_name(&self, encrypted: &[u8]) -> Option<String> {
        chatter_linux::crypto::profile_crypto::decrypt_display_name(
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
        chatter_linux::crypto::derive_uak(&self.profile_key)
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

/// Generate a random password and return (password, sha256_hash).
pub fn random_password() -> (String, Vec<u8>) {
    let mut random_bytes = [0u8; 16];
    rand::rng().fill_bytes(&mut random_bytes);
    let password = format!("TestPass_{}", hex::encode(random_bytes));
    let hash = Sha256::digest(password.as_bytes()).to_vec();
    (password, hash)
}

/// Generate a random 32-byte hash (for password_hash parameter).
pub fn random_password_hash() -> Vec<u8> {
    let mut hash = vec![0u8; 32];
    rand::rng().fill_bytes(&mut hash);
    hash
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
    let display_name = format!("Test User {}", &username[10..18]);
    let (password, password_hash) = random_password();
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
    let signed_prekey_timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;

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
        unidentified_access_key: Some(random_uak().to_vec()),
        unrestricted_unidentified_access: Some(false),
    };

    let mut auth_client = AuthClient::new(pool.channel(), pool.auth_clone());

    // Encrypt display name with profile key (required for Phase 79 privacy)
    let encrypted_display_name = chatter_linux::crypto::profile_crypto::encrypt_display_name(
        &profile_key,
        &display_name,
    )
    .expect("Display name encryption should succeed");

    // Generate unidentified access key (16 bytes) for sealed sender
    let uak = random_uak();

    let result = auth_client
        .register_and_wait(
            &username,
            &password_hash,
            &encrypted_display_name,
            identity_public_key.as_ref(),
            "integration-test-device",
            &device_auth_token,
            &uak,
            Some(prekey_bundle),
            Vec::new(), // No additional one-time prekeys for tests
            "",         // No challenge ID for tests
            &[],        // No challenge response for tests
            &[0u8; 32], // Empty registration lock for tests
        )
        .await?;

    Ok(TestUser {
        username,
        display_name,
        encrypted_display_name,
        password_hash,
        password,
        public_key: identity_public_key.as_ref().to_vec(),
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

/// Create a test user fixture without registering (for unit tests).
pub fn mock_test_user() -> TestUser {
    let username = unique_username();
    let (password, password_hash) = random_password();
    let profile_key = random_profile_key();
    let display_name = format!("Mock User {}", &username[10..18.min(username.len())]);

    // Encrypt the display name with the profile key
    let encrypted_display_name = chatter_linux::crypto::profile_crypto::encrypt_display_name(
        &profile_key,
        &display_name,
    )
    .expect("Display name encryption should succeed");

    TestUser {
        username,
        display_name,
        encrypted_display_name,
        password_hash,
        password,
        public_key: random_public_key(),
        device_auth_token: random_device_auth_token(),
        profile_key,
        user_id: uuid::Uuid::new_v4().to_string(),
        device_id: 1,
    }
}

/// Encrypt a display name for testing.
///
/// Uses the provided profile key to encrypt the display name.
/// Returns raw encrypted bytes (nonce || ciphertext || auth_tag).
pub fn encrypt_test_display_name(profile_key: &[u8; 32], display_name: &str) -> Vec<u8> {
    chatter_linux::crypto::profile_crypto::encrypt_display_name(profile_key, display_name)
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
    let signed_prekey_timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;

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
        unidentified_access_key: Some(random_uak().to_vec()),
        unrestricted_unidentified_access: Some(false),
    };

    (prekey_bundle, identity_public_key.as_ref().to_vec())
}

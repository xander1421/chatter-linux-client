//! Crypto builders for Signal protocol test data.

use crate::framework::fixtures::{TestAttachment, TestSession, random_bytes, random_profile_key};
use rand::RngCore;
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;

/// Builder for cryptographic test data.
pub struct CryptoBuilder;

impl CryptoBuilder {
    /// Generate a random identity key pair (private, public).
    pub fn identity_key_pair() -> (Vec<u8>, Vec<u8>) {
        let private = random_bytes(32);
        let mut public = vec![0x05u8]; // DJB key type
        public.extend(random_bytes(32));
        (private, public)
    }

    /// Generate a random prekey.
    pub fn prekey(id: u32) -> (u32, Vec<u8>, Vec<u8>) {
        let private = random_bytes(32);
        let mut public = vec![0x05u8];
        public.extend(random_bytes(32));
        (id, private, public)
    }

    /// Generate a random signed prekey with signature.
    pub fn signed_prekey(id: u32) -> (u32, Vec<u8>, Vec<u8>, Vec<u8>) {
        let private = random_bytes(32);
        let mut public = vec![0x05u8];
        public.extend(random_bytes(32));
        let signature = random_bytes(64);
        (id, private, public, signature)
    }

    /// Generate a random Kyber prekey.
    pub fn kyber_prekey(id: u32) -> (u32, Vec<u8>, Vec<u8>) {
        let public = random_bytes(1568); // Kyber1024 public key size
        let signature = random_bytes(64);
        (id, public, signature)
    }

    /// Generate a random registration ID.
    pub fn registration_id() -> u32 {
        rand::random::<u32>() % 16380 + 1
    }

    /// Generate a random sender certificate.
    pub fn sender_certificate() -> Vec<u8> {
        random_bytes(256)
    }

    /// Generate random ciphertext.
    pub fn ciphertext(len: usize) -> Vec<u8> {
        random_bytes(len)
    }

    /// Generate a random message key.
    pub fn message_key() -> Vec<u8> {
        random_bytes(32)
    }

    /// Generate a random chain key.
    pub fn chain_key() -> Vec<u8> {
        random_bytes(32)
    }

    /// Generate random sender key.
    pub fn sender_key() -> Vec<u8> {
        random_bytes(32)
    }

    /// Generate random distribution ID.
    pub fn distribution_id() -> Vec<u8> {
        uuid::Uuid::new_v4().as_bytes().to_vec()
    }

    /// Generate a random 32-byte profile key.
    pub fn profile_key() -> [u8; 32] {
        random_profile_key()
    }

    /// Encrypt a display name using the profile key.
    ///
    /// Returns encrypted bytes (nonce || ciphertext || auth_tag).
    pub fn encrypt_display_name(profile_key: &[u8; 32], display_name: &str) -> Vec<u8> {
        chatter_linux::crypto::profile_crypto::encrypt_display_name(profile_key, display_name)
            .expect("Display name encryption should succeed")
    }

    /// Encrypt a display name and return as base64 string (for API calls).
    pub fn encrypt_display_name_base64(profile_key: &[u8; 32], display_name: &str) -> String {
        let encrypted = Self::encrypt_display_name(profile_key, display_name);
        BASE64.encode(&encrypted)
    }

    /// Decrypt a display name using the profile key.
    ///
    /// Returns the plaintext display name.
    pub fn decrypt_display_name(profile_key: &[u8; 32], encrypted: &[u8]) -> Option<String> {
        chatter_linux::crypto::profile_crypto::decrypt_display_name(profile_key, encrypted).ok()
    }

    /// Decrypt a base64-encoded display name.
    pub fn decrypt_display_name_base64(profile_key: &[u8; 32], encrypted_base64: &str) -> Option<String> {
        let encrypted = BASE64.decode(encrypted_base64).ok()?;
        Self::decrypt_display_name(profile_key, &encrypted)
    }

    /// Generate a fake encrypted display name (valid format, random content).
    ///
    /// Useful for tests that don't need real encryption.
    /// Returns raw bytes (156 bytes = 12 nonce + 128 padded + 16 tag).
    pub fn fake_encrypted_display_name() -> Vec<u8> {
        random_bytes(156)
    }

    /// Generate a fake encrypted display name as base64.
    pub fn fake_encrypted_display_name_base64() -> String {
        BASE64.encode(&Self::fake_encrypted_display_name())
    }

    /// Derive Unidentified Access Key from profile key.
    pub fn derive_uak(profile_key: &[u8; 32]) -> [u8; 16] {
        chatter_linux::crypto::derive_uak(profile_key)
    }
}

/// Builder for Signal sessions.
pub struct SessionBuilder {
    user_id: String,
    device_id: u32,
    established: bool,
    session_data: Option<Vec<u8>>,
}

impl SessionBuilder {
    /// Create a new session builder.
    pub fn new(user_id: impl Into<String>, device_id: u32) -> Self {
        Self {
            user_id: user_id.into(),
            device_id,
            established: false,
            session_data: None,
        }
    }

    /// Mark session as established.
    pub fn established(mut self) -> Self {
        self.established = true;
        self.session_data = Some(random_bytes(256));
        self
    }

    /// Set custom session data.
    pub fn with_data(mut self, data: Vec<u8>) -> Self {
        self.session_data = Some(data);
        self.established = true;
        self
    }

    /// Build the session.
    pub fn build(self) -> TestSession {
        TestSession {
            user_id: self.user_id,
            device_id: self.device_id,
            session_data: self.session_data.unwrap_or_default(),
            established: self.established,
        }
    }

    /// Build multiple sessions for different devices.
    pub fn build_multi_device(user_id: impl Into<String>, device_count: u32) -> Vec<TestSession> {
        let user_id = user_id.into();
        (1..=device_count)
            .map(|d| Self::new(user_id.clone(), d).established().build())
            .collect()
    }
}

/// Builder for attachments.
pub struct AttachmentBuilder {
    filename: String,
    mime_type: String,
    size: u64,
    cdn_url: Option<String>,
}

impl AttachmentBuilder {
    /// Create an image attachment.
    pub fn image(filename: impl Into<String>) -> Self {
        Self {
            filename: filename.into(),
            mime_type: "image/jpeg".into(),
            size: 100 * 1024, // 100KB
            cdn_url: None,
        }
    }

    /// Create a video attachment.
    pub fn video(filename: impl Into<String>) -> Self {
        Self {
            filename: filename.into(),
            mime_type: "video/mp4".into(),
            size: 10 * 1024 * 1024, // 10MB
            cdn_url: None,
        }
    }

    /// Create an audio attachment.
    pub fn audio(filename: impl Into<String>) -> Self {
        Self {
            filename: filename.into(),
            mime_type: "audio/mp3".into(),
            size: 5 * 1024 * 1024, // 5MB
            cdn_url: None,
        }
    }

    /// Create a document attachment.
    pub fn document(filename: impl Into<String>) -> Self {
        Self {
            filename: filename.into(),
            mime_type: "application/pdf".into(),
            size: 1024 * 1024, // 1MB
            cdn_url: None,
        }
    }

    /// Set custom MIME type.
    pub fn mime_type(mut self, mime: impl Into<String>) -> Self {
        self.mime_type = mime.into();
        self
    }

    /// Set file size.
    pub fn size(mut self, size: u64) -> Self {
        self.size = size;
        self
    }

    /// Set CDN URL.
    pub fn cdn_url(mut self, url: impl Into<String>) -> Self {
        self.cdn_url = Some(url.into());
        self
    }

    /// Build the attachment.
    pub fn build(self) -> TestAttachment {
        let mut key = vec![0u8; 32];
        let mut digest = vec![0u8; 32];
        rand::rng().fill_bytes(&mut key);
        rand::rng().fill_bytes(&mut digest);

        TestAttachment {
            id: uuid::Uuid::new_v4().to_string(),
            filename: self.filename,
            mime_type: self.mime_type,
            size: self.size,
            key,
            digest,
            cdn_url: self.cdn_url,
        }
    }
}

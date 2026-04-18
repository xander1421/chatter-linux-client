//! Authentication crypto helpers.
//!
//! Password hashing, profile key derivation, and username hashing. These are
//! the three primitives shared by login, registration, and recovery flows.
//!
//! # Privacy
//!
//! Usernames are never sent to the server in plaintext. Instead, the client
//! derives a profile key from `(password, username)` via HKDF-SHA256, then
//! HMAC-SHA256s the normalized username with that key. The server only ever
//! sees the resulting hex digest.
//!
//! # Determinism
//!
//! `(password, username) -> username_hash` is deterministic, so a user can
//! log in from any device with only those two inputs.

use hkdf::Hkdf;
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

use crate::error::CryptoError;

/// Hash a password with SHA-256.
///
/// Used as IKM for [`derive_profile_key`] and as the credential sent to the
/// server (which stores only its own Argon2id-derived digest).
#[must_use]
pub fn hash_password(password: &str) -> [u8; 32] {
    Sha256::digest(password.as_bytes()).into()
}

/// Derive a 32-byte profile key from a password hash and username.
///
/// Uses HKDF-SHA256 with `info = "chatter_profile_key_{username}"` for domain
/// separation. Output is suitable for HMAC-SHA256 and AES-256-GCM.
pub fn derive_profile_key(password_hash: &[u8], username: &str) -> Result<[u8; 32], CryptoError> {
    let hk = Hkdf::<Sha256>::new(None, password_hash);
    let info = format!("chatter_profile_key_{}", username.trim().to_lowercase());
    let mut profile_key = [0u8; 32];
    hk.expand(info.as_bytes(), &mut profile_key)
        .map_err(|_| CryptoError::KeyDerivationFailed("HKDF expand failed for profile key".into()))?;
    Ok(profile_key)
}

/// Derive a 12-byte (96-bit) delivery token from a profile key.
///
/// Signal-style `HKDF-SHA256(profile_key, info = "DeliveryToken")`. Used in
/// private friend codes and sealed-sender delivery so recipients can be
/// reached without leaking profile keys to the server.
#[must_use]
pub fn derive_delivery_token(profile_key: &[u8; 32]) -> [u8; 12] {
    let hk = Hkdf::<Sha256>::new(None, profile_key);
    let mut token = [0u8; 12];
    hk.expand(b"DeliveryToken", &mut token)
        .expect("HKDF expand cannot fail for 12-byte output");
    token
}

/// Hash a username with HMAC-SHA256 keyed by the profile key, hex-encoded.
///
/// Server never sees plaintext usernames; only this digest. Same `(password,
/// username)` pair always yields the same hash so login works from any device.
///
/// This is the **auth hash**: scoped to the owner's profile key so another
/// user cannot derive it without that key. For Signal-style cross-user
/// discovery use [`hash_username_lookup`] instead.
#[must_use]
pub fn hash_username(username: &str, profile_key: &[u8; 32]) -> String {
    type HmacSha256 = Hmac<Sha256>;

    let normalized = username.trim().to_lowercase();
    let mut mac = HmacSha256::new_from_slice(profile_key)
        .expect("HMAC can take key of any size");
    mac.update(normalized.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

/// Fixed salt for the Signal-style username-discovery hash. Bumping the `v1`
/// suffix re-rolls every registered account's lookup hash (requires a
/// coordinated server-side re-hash).
const USERNAME_LOOKUP_SALT: &[u8] = b"chatter/username-discovery/v1";

/// Derive a 32-byte Signal-style discovery hash of a username.
///
/// Unlike [`hash_username`], this hash is **not** scoped to a profile key —
/// any client that knows `alice.42` can reproduce it and ask the server
/// `LookupUsername(hash) -> ACI`. That is the intended asymmetry: the auth
/// hash protects login, the lookup hash makes the account findable.
///
/// HKDF-SHA256 with:
/// * salt = [`USERNAME_LOOKUP_SALT`] (domain separator, global constant)
/// * IKM  = lowercased, trimmed UTF-8 of the username
/// * info = `b"lookup"`
///
/// The discovery hash is lightweight (not Argon2) because the server must
/// rate-limit and quota the lookup endpoint anyway — a slow KDF on the
/// client adds no extra protection against an attacker who can already
/// query the server.
#[must_use]
pub fn hash_username_lookup(username: &str) -> [u8; 32] {
    let normalized = username.trim().to_lowercase();
    let hk = Hkdf::<Sha256>::new(Some(USERNAME_LOOKUP_SALT), normalized.as_bytes());
    let mut out = [0u8; 32];
    hk.expand(b"lookup", &mut out)
        .expect("HKDF expand cannot fail for 32-byte output");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_key_is_deterministic() {
        let pw = hash_password("hunter2");
        let k1 = derive_profile_key(&pw, "Alice").unwrap();
        let k2 = derive_profile_key(&pw, "alice  ").unwrap();
        assert_eq!(k1, k2, "username normalization (trim + lowercase) must apply");
    }

    #[test]
    fn profile_key_differs_by_password() {
        let k1 = derive_profile_key(&hash_password("a"), "bob").unwrap();
        let k2 = derive_profile_key(&hash_password("b"), "bob").unwrap();
        assert_ne!(k1, k2);
    }

    #[test]
    fn username_hash_is_deterministic() {
        let pw = hash_password("pw");
        let pk = derive_profile_key(&pw, "carol").unwrap();
        let h1 = hash_username("carol", &pk);
        let h2 = hash_username("  CAROL ", &pk);
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 64, "hex-encoded SHA-256 digest");
    }

    #[test]
    fn lookup_hash_is_profile_key_free_and_normalized() {
        let h1 = hash_username_lookup("dave");
        let h2 = hash_username_lookup("  DAVE ");
        assert_eq!(h1, h2);
        let h3 = hash_username_lookup("eve");
        assert_ne!(h1, h3);
    }
}

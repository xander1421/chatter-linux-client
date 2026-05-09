//! Authentication crypto helpers.
//!
//! Primitives shared by login, registration, and recovery:
//! - [`hash_password`]: SHA-256 of password (sent to server as auth credential)
//! - [`generate_profile_key`]: random 32 bytes (Signal-style)
//! - [`derive_login_auth_hash`]: deterministic, password-keyed login lookup
//! - [`hash_username_lookup`]: deterministic, username-only discovery hash
//! - [`derive_delivery_token`]: 96-bit profile-key-derived sealed-sender token
//!
//! # Privacy
//!
//! Usernames are never sent to the server in plaintext. The login lookup
//! hash is keyed by `SHA-256(password)`, so an unauthenticated party cannot
//! enumerate accounts by username alone.
//!
//! # Determinism
//!
//! `(password, username) -> login_auth_hash` is deterministic, so a user
//! can authenticate from any device with only those two inputs. The profile
//! key is NOT deterministic from password (Signal-aligned); fresh devices
//! recover it from SVR backup using PIN.

use hkdf::Hkdf;
use rand::TryRngCore;
use rand::rngs::OsRng;
use sha2::{Digest, Sha256};

/// Hash a password with SHA-256.
///
/// Used as the auth credential sent to the server (which stores only its own
/// Argon2id-derived digest of these bytes) and as IKM for
/// [`derive_login_auth_hash`].
///
/// NOTE: This output must NEVER be used as IKM for the profile key. Profile
/// keys are random ([`generate_profile_key`]); deriving them from password
/// material would mean a server-side breach + Argon2 crack recovers the
/// profile key for every account. See Signal-Android `ProfileKeyUtil`.
#[must_use]
pub fn hash_password(password: &str) -> [u8; 32] {
    Sha256::digest(password.as_bytes()).into()
}

/// Generate a fresh 32-byte profile key from the OS CSPRNG.
///
/// Signal-aligned: profile keys are random, NOT derived from password, PIN,
/// or any deterministic input. The key is persisted locally and (in the
/// multi-device case) wrapped under a PIN-derived key in the SVR backup so
/// other devices can recover it. Reference:
/// `Signal-Android/app/src/main/java/org/thoughtcrime/securesms/crypto/ProfileKeyUtil.java`
/// `return new ProfileKey(Util.getSecretBytes(32));`
#[must_use]
pub fn generate_profile_key() -> [u8; 32] {
    let mut k = [0u8; 32];
    OsRng
        .try_fill_bytes(&mut k)
        .expect("OS CSPRNG must not fail");
    k
}

/// Derive the per-account login lookup hash, independent of the profile key.
///
/// Server stores this 32-byte hash in the username column and looks accounts
/// up by it during login/register. Keyed off `(SHA-256(password), username)`
/// so any device with the same credentials reproduces it - no profile_key
/// chicken-and-egg on a fresh device.
///
/// HKDF-SHA256:
///   salt = `b"chatter/login-auth/v1"` (domain separator)
///   IKM  = SHA-256(password)
///   info = `b"login-auth:" || lowercased(trim(username))`
///
/// Why this replaces the old `hash_username(username, profile_key)`: profile
/// keys are now random, so a profile-key-keyed lookup hash would require the
/// client to fetch the SVR backup BEFORE login - but SVR fetch is itself
/// authenticated. This derivation breaks the cycle.
#[must_use]
pub fn derive_login_auth_hash(password_hash: &[u8], username: &str) -> [u8; 32] {
    let hk = Hkdf::<Sha256>::new(Some(b"chatter/login-auth/v1"), password_hash);
    let info = format!("login-auth:{}", username.trim().to_lowercase());
    let mut out = [0u8; 32];
    hk.expand(info.as_bytes(), &mut out)
        .expect("HKDF expand cannot fail for 32-byte output");
    out
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

/// Fixed salt for the Signal-style username-discovery hash. Bumping the `v1`
/// suffix re-rolls every registered account's lookup hash (requires a
/// coordinated server-side re-hash).
const USERNAME_LOOKUP_SALT: &[u8] = b"chatter/username-discovery/v1";

/// Derive a 32-byte Signal-style discovery hash of a username.
///
/// **Discovery only.** Use [`derive_login_auth_hash`] for login lookup -
/// the discovery hash is scoped to (username) alone so any party can resolve
/// `alice.42 -> ACI`, while login lookup must additionally bind the password
/// so server-side enumeration cannot map names to accounts without proof of
/// possession.
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
    fn profile_key_is_random() {
        let k1 = generate_profile_key();
        let k2 = generate_profile_key();
        assert_ne!(k1, k2, "two CSPRNG draws must differ with overwhelming probability");
        assert_ne!(k1, [0u8; 32], "must not be all zeros");
    }

    #[test]
    fn login_auth_hash_is_deterministic_and_normalized() {
        let pw = hash_password("hunter2");
        let h1 = derive_login_auth_hash(&pw, "Alice");
        let h2 = derive_login_auth_hash(&pw, "alice  ");
        assert_eq!(h1, h2, "username trim+lowercase normalization must apply");
        assert_eq!(h1.len(), 32);
    }

    #[test]
    fn login_auth_hash_differs_by_password() {
        let h1 = derive_login_auth_hash(&hash_password("a"), "bob");
        let h2 = derive_login_auth_hash(&hash_password("b"), "bob");
        assert_ne!(h1, h2);
    }

    #[test]
    fn login_auth_hash_differs_by_username() {
        let pw = hash_password("same");
        let h1 = derive_login_auth_hash(&pw, "alice");
        let h2 = derive_login_auth_hash(&pw, "bob");
        assert_ne!(h1, h2);
    }

    #[test]
    fn login_auth_hash_independent_of_lookup_hash() {
        // Critical invariant: login auth hash MUST NOT equal the public
        // discovery hash. If they collided, any party who can resolve
        // username->ACI could also forge login lookup keys.
        let pw = hash_password("p");
        let auth = derive_login_auth_hash(&pw, "alice");
        let lookup = hash_username_lookup("alice");
        assert_ne!(auth, lookup);
    }

    #[test]
    fn lookup_hash_is_normalized() {
        let h1 = hash_username_lookup("dave");
        let h2 = hash_username_lookup("  DAVE ");
        assert_eq!(h1, h2);
        let h3 = hash_username_lookup("eve");
        assert_ne!(h1, h3);
    }
}

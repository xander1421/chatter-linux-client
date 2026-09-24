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

/// Reduce what a user types at login to the nickname registration keyed on.
///
/// Registration derives the login lookup hash and the SVR PIN salt from the
/// bare nickname (`AuthService::register` hashes the typed `username`; the
/// `.discriminator` is chosen afterwards by [`username_candidates`]). The app
/// shows the full handle as `@nickname.discriminator`, so users type that
/// too. Strips a leading `@` and a trailing `.digits` discriminator; nothing
/// else. Safe because a nickname cannot contain `.` (libsignal's grammar is
/// `[_0-9a-z]`), so no registered nickname is ever shortened by this.
#[must_use]
pub fn login_nickname(input: &str) -> &str {
    let s = input.trim();
    let s = s.strip_prefix('@').unwrap_or(s);
    match s.rsplit_once('.') {
        Some((nick, disc)) if !disc.is_empty() && disc.bytes().all(|b| b.is_ascii_digit()) => nick,
        _ => s,
    }
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

/// Compute the Signal-compatible username discovery hash.
///
/// `username` must be in `nickname.discriminator` form (e.g. `alice.4711`).
/// The discriminator is picked at registration via [`username_candidates`]
/// and makes wordlist enumeration of registered usernames infeasible: even
/// with a complete dictionary of nicknames, an attacker has to grind 2-5
/// digits of discriminator per nickname per query.
///
/// Returns the 32-byte Ristretto255 compressed point that libsignal's
/// `usernames::Username` uses as its lookup hash. Verification of ownership
/// (the user proves they know the secret scalars behind the point) is
/// handled by [`username_proof`] / [`verify_username_proof`].
///
/// # Errors
/// Returns the underlying `usernames::UsernameError` if `username` does not
/// match Signal's nickname grammar (length, charset, separator presence,
/// discriminator range).
pub fn hash_username_lookup(username: &str) -> Result<[u8; 32], usernames::UsernameError> {
    Ok(usernames::Username::new(username)?.hash())
}

/// Pick a set of `nickname.discriminator` candidates for registration.
///
/// libsignal's `Username::candidates_from` generates several candidates in
/// the standard discriminator ranges (`[0-99, 100-999, ...]`). The caller
/// tries them in order at registration; the first one not already taken
/// (server-enforced uniqueness on the lookup hash) wins.
pub fn username_candidates(nickname: &str) -> Result<Vec<String>, usernames::UsernameError> {
    let mut rng = rand::rng();
    let limits = usernames::NicknameLimits::new(3, 32);
    usernames::Username::candidates_from(&mut rng, nickname, limits)
}

/// Generate a zero-knowledge proof that the caller owns `username`.
///
/// The verifier learns only the lookup hash; the caller proves possession
/// of the secret scalars committed to by the hash. Used by username-change
/// flows, profile-link verification, etc.
pub fn username_proof(username: &str) -> Result<Vec<u8>, usernames::UsernameError> {
    let mut randomness = [0u8; 32];
    OsRng
        .try_fill_bytes(&mut randomness)
        .expect("OS CSPRNG must not fail");
    usernames::Username::new(username)?.proof(&randomness)
}

/// Verify a zero-knowledge proof of ownership produced by [`username_proof`].
pub fn verify_username_proof(
    proof: &[u8],
    hash: [u8; 32],
) -> Result<(), usernames::ProofVerificationFailure> {
    usernames::Username::verify_proof(proof, hash)
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
    fn login_nickname_accepts_every_shown_form() {
        for typed in ["carolsweep", "carolsweep.76", "@carolsweep.76", "@carolsweep", "  carolsweep.76 "] {
            assert_eq!(login_nickname(typed), "carolsweep", "{typed:?}");
        }
        // Not a discriminator: left alone so the server rejects it as typed.
        assert_eq!(login_nickname("carol.sweep"), "carol.sweep");
        assert_eq!(login_nickname("carolsweep."), "carolsweep.");
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
        let lookup = hash_username_lookup("alice.42").unwrap();
        assert_ne!(auth, lookup);
    }

    #[test]
    fn lookup_hash_requires_discriminator() {
        // Plain nickname without `.discriminator` must be rejected -
        // forgetting to attach a discriminator was the whole pre-fix bug
        // (trivially enumerable by wordlist).
        assert!(hash_username_lookup("dave").is_err());
        assert!(hash_username_lookup("dave.").is_err());
    }

    #[test]
    fn lookup_hash_changes_with_discriminator() {
        let h1 = hash_username_lookup("dave.42").unwrap();
        let h2 = hash_username_lookup("dave.43").unwrap();
        let h3 = hash_username_lookup("eve.42").unwrap();
        assert_ne!(h1, h2, "different discriminators yield different hashes");
        assert_ne!(h1, h3, "different nicknames yield different hashes");
    }

    #[test]
    fn proof_verifies_for_owner_only() {
        let proof = username_proof("alice.42").unwrap();
        let hash = hash_username_lookup("alice.42").unwrap();
        assert!(verify_username_proof(&proof, hash).is_ok());

        let other_hash = hash_username_lookup("alice.43").unwrap();
        assert!(verify_username_proof(&proof, other_hash).is_err());
    }

    #[test]
    fn candidates_are_well_formed() {
        let cands = username_candidates("alice").unwrap();
        assert!(!cands.is_empty());
        for c in &cands {
            assert!(c.starts_with("alice."));
            // Every candidate must round-trip through Username::new.
            assert!(hash_username_lookup(c).is_ok(), "invalid candidate: {c}");
        }
    }
}

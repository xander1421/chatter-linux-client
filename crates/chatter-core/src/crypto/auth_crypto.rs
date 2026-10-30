//! Authentication crypto helpers.
//!
//! Primitives shared by login, registration, and recovery:
//! - [`login_identifier`]: the nickname-only login identifier the server
//!   indexes accounts by (the password itself is handled by OPAQUE,
//!   [`crate::crypto::opaque`], and never reaches the server)
//! - [`generate_profile_key`]: random 32 bytes (Signal-style)
//! - [`hash_username_lookup`]: deterministic, username-only discovery hash
//! - [`derive_delivery_token`]: 96-bit profile-key-derived sealed-sender token
//!
//! # Privacy
//!
//! Usernames are never sent to the server in plaintext. The login identifier
//! is a hash of the nickname alone; the profile key is NOT derived from the
//! password (Signal-aligned) and fresh devices recover it from SVR backup
//! using the PIN.

use hkdf::Hkdf;
use rand::TryRngCore;
use rand::rngs::OsRng;
use sha2::{Digest, Sha256};
use zeroize::Zeroize;

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

/// The login identifier for a nickname: `hex(SHA-256("chatter/login-id/v1"
/// || lower(trim(nickname))))`, the `username` the server indexes accounts by
/// (users.username) and keys the per-account login lockout on.
///
/// Derived from the nickname ALONE. It used to mix in the password, which
/// kept a database dump from enumerating nicknames but made the identifier
/// itself a password-guessing oracle; OPAQUE needs neither. The cost, stated
/// plainly: anyone can compute this for a guessed nickname, so a database dump
/// reveals which guessed nicknames exist (by dictionary), and the per-account
/// lockout is keyed on a value an attacker can name. Online, existence is not
/// observable: the server answers an unknown identifier from a fake OPAQUE
/// record (RFC 9807 §6.3.2.2), indistinguishable without the password.
///
/// `nickname` must be the bare registration nickname (what [`login_nickname`]
/// returns and `keys::SVR_SALT_USERNAME` stores).
#[must_use]
pub fn login_identifier(nickname: &str) -> String {
    let mut h = Sha256::new();
    h.update(b"chatter/login-id/v1");
    h.update(nickname.trim().to_lowercase().as_bytes());
    hex::encode(h.finalize())
}

/// Reduce what a user types at login to the nickname registration keyed on.
///
/// Registration derives the login identifier and the SVR PIN salt from the
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
/// handled by [`username_claim`] / [`verify_username_proof`].
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

/// The username fields of a `RegisterRequest`: the discovery hash and a
/// zero-knowledge proof that the caller knows the username behind it.
///
/// The server verifies the proof against the hash before storing it (as
/// Signal-Server's AccountController does with `Username.verifyProof`) and
/// rejects registration without one, so both come from one parse of
/// `username` and travel together.
pub fn username_claim(username: &str) -> Result<([u8; 32], Vec<u8>), usernames::UsernameError> {
    let username = usernames::Username::new(username)?;
    // libsignal's prover takes 32 bytes of caller-supplied randomness for the
    // poksho nonce; it must be fresh per proof.
    let mut randomness = [0u8; 32];
    OsRng
        .try_fill_bytes(&mut randomness)
        .expect("OS CSPRNG must not fail");
    let proof = username.proof(&randomness)?;
    randomness.zeroize();
    Ok((username.hash(), proof))
}

/// Verify a zero-knowledge proof of ownership produced by [`username_claim`].
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
        assert_ne!(
            k1, k2,
            "two CSPRNG draws must differ with overwhelming probability"
        );
        assert_ne!(k1, [0u8; 32], "must not be all zeros");
    }

    #[test]
    fn login_nickname_accepts_every_shown_form() {
        for typed in [
            "carolsweep",
            "carolsweep.76",
            "@carolsweep.76",
            "@carolsweep",
            "  carolsweep.76 ",
        ] {
            assert_eq!(login_nickname(typed), "carolsweep", "{typed:?}");
        }
        // Not a discriminator: left alone so the server rejects it as typed.
        assert_eq!(login_nickname("carol.sweep"), "carol.sweep");
        assert_eq!(login_nickname("carolsweep."), "carolsweep.");
    }

    const LOGIN_ID_ALICE_KAT: &str =
        "161261e5f314e189b1a590cb4ceaaa3bb823366d22ae3ee1f44b9ed0ee20b2dc";

    #[test]
    fn login_identifier_is_nickname_only_normalized_and_pinned() {
        assert_eq!(login_identifier("Alice"), login_identifier("  alice "));
        assert_ne!(login_identifier("alice"), login_identifier("bob"));
        let id = login_identifier("alice");
        assert_eq!(id.len(), 64, "server pattern ^[a-zA-Z0-9_-]+$, max 64");
        assert!(id.bytes().all(|b| b.is_ascii_hexdigit()));
        // Pins the construction: changing it moves every account's identifier.
        // Independently computed: printf 'chatter/login-id/v1alice' | sha256sum
        assert_eq!(id, LOGIN_ID_ALICE_KAT);
        // Never collides with the public discovery hash.
        assert_ne!(id, hex::encode(hash_username_lookup("alice.42").unwrap()));
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
        let (hash, proof) = username_claim("alice.42").unwrap();
        assert_eq!(hash, hash_username_lookup("alice.42").unwrap());
        assert!(verify_username_proof(&proof, hash).is_ok());

        let other_hash = hash_username_lookup("alice.43").unwrap();
        assert!(verify_username_proof(&proof, other_hash).is_err());
    }

    /// The two username fields `AuthClient::register` puts on the wire must,
    /// after a protobuf round trip, verify the way the server verifies them
    /// (internal/services/auth/service/user.go → libsignal verify_proof), and
    /// fit the proto's `username_proof` length rule (128 bytes).
    #[test]
    fn register_request_username_proof_verifies() {
        use prost::Message;
        let (hash, proof) = username_claim("alice.42").unwrap();
        let req = crate::proto::chat_v1::RegisterRequest {
            username_lookup_hash: hash.to_vec(),
            username_proof: proof,
            ..Default::default()
        };
        let wire =
            crate::proto::chat_v1::RegisterRequest::decode(req.encode_to_vec().as_slice()).unwrap();
        assert_eq!(wire.username_proof.len(), 128);
        let wire_hash: [u8; 32] = wire.username_lookup_hash.as_slice().try_into().unwrap();
        assert!(verify_username_proof(&wire.username_proof, wire_hash).is_ok());

        let mut tampered = wire.username_proof.clone();
        tampered[40] ^= 1;
        assert!(verify_username_proof(&tampered, wire_hash).is_err());
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

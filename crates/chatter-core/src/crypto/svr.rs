//! Secure Value Recovery (SVR) cryptography.
//!
//! Handles derivation of the SVR keypair from a user-provided PIN.
//! Uses Argon2id to provide memory-hard derivation, protecting against brute-force
//! attacks on short PINs. The username is used as a deterministic salt.

use argon2::{Argon2, Params};
use hmac::{Hmac, Mac};
use sha2::Sha256;
use x25519_dalek::{PublicKey, StaticSecret};
use zeroize::Zeroize;

use crate::error::CryptoError;

/// Minimum PIN length enforced at the crypto layer.
///
/// 8 ASCII characters at ~36 alphabet entropy give ~41 bits of password
/// entropy. Without an enclave protecting the SVR backup, this is the user's
/// only defense against an attacker who has stolen the wrapped backup blob:
/// the search space size dictates the offline brute-force cost. Coupled with
/// [`get_argon2_params`] below, 41 bits × ~10^9 Argon2 ops/sec/GPU pushes a
/// full-keyspace search to multi-decade scale, which is the best a software-
/// only deployment can do. A real enclave (SGX/Nitro/etc.) would let us drop
/// this to 4-6 chars while remaining safe.
pub const MIN_PIN_LENGTH: usize = 8;
pub const MAX_PIN_LENGTH: usize = 64;

/// Argon2id parameters for PIN derivation.
///
/// `m=128 MiB`, `t=8`, `p=4`, output = 32 bytes.
///
/// **Why these numbers (no-enclave deployment):** every Argon2 invocation
/// must dominate offline brute-force cost. Bumped from the prior `(64 MiB,
/// 3)` because in a software-only deployment the SVR backup is plain
/// ciphertext on Postgres, so an attacker who steals the DB does an offline
/// attack against (PIN entropy × Argon2 work). The current numbers cost
/// roughly ~2-3 seconds per derivation on a modern mobile device and
/// multiple seconds of GPU time per attacker guess. With [`MIN_PIN_LENGTH`]
/// = 8, the combined cost makes full-keyspace search infeasible on
/// commodity hardware.
///
/// If you switch to an enclave-backed SVR (see SECURITY.md, "SVR enclave
/// gap"), these can drop back to (64 MiB, 3) - the enclave provides the
/// throttling guarantee instead of work factor.
fn get_argon2_params() -> Params {
    Params::new(
        131072,   // m_cost (128 MiB)
        8,        // t_cost (8 iterations)
        4,        // p_cost (4 lanes)
        Some(32), // output length (32 bytes)
    )
    .expect("Valid Argon2 parameters")
}

/// Minimum estimated entropy (bits) for a PIN.
///
/// The SVR backup wrapping the identity + profile key is protected only by
/// Argon2id(PIN) with no enclave throttling yet (see SECURITY.md "SVR enclave
/// gap"), so a stolen blob is brute-forced offline. This floor rejects the
/// weakest common case — an 8-digit numeric PIN (~26.6 bits) — while still
/// accepting an 8-char alphanumeric PIN (~37.6 bits). It is a tunable
/// mitigation, not the cure; the cure is enclave-backed SVR.
pub const MIN_PIN_ENTROPY_BITS: f64 = 30.0;

/// Estimate PIN entropy as `len * log2(charset)`, where `charset` grows with
/// the character classes present (digits, lower, upper, symbols). This is a
/// deliberate over-estimate (assumes uniform random over the used classes);
/// it only needs to distinguish "trivially small keyspace" from "not".
fn estimate_pin_entropy_bits(pin: &str) -> f64 {
    let mut charset = 0u32;
    if pin.chars().any(|c| c.is_ascii_digit()) {
        charset += 10;
    }
    if pin.chars().any(|c| c.is_ascii_lowercase()) {
        charset += 26;
    }
    if pin.chars().any(|c| c.is_ascii_uppercase()) {
        charset += 26;
    }
    if pin.chars().any(|c| c.is_ascii_graphic() && !c.is_ascii_alphanumeric()) {
        charset += 32;
    }
    if charset == 0 {
        return 0.0;
    }
    pin.chars().count() as f64 * (charset as f64).log2()
}

/// Validate a PIN's shape and strength.
///
/// `MIN_PIN_LENGTH..=MAX_PIN_LENGTH` ASCII printable bytes, and at least
/// [`MIN_PIN_ENTROPY_BITS`] of estimated entropy. The entropy floor rejects
/// low-keyspace PINs (e.g. an 8-digit numeric PIN) that a software-only SVR
/// cannot otherwise defend against.
pub fn validate_pin(pin: &str) -> Result<(), CryptoError> {
    if pin.len() < MIN_PIN_LENGTH {
        return Err(CryptoError::Validation(format!(
            "PIN must be at least {MIN_PIN_LENGTH} characters",
        )));
    }
    if pin.len() > MAX_PIN_LENGTH {
        return Err(CryptoError::Validation(format!(
            "PIN must be at most {MAX_PIN_LENGTH} characters",
        )));
    }
    if !pin.chars().all(|c| c.is_ascii_graphic()) {
        return Err(CryptoError::Validation(
            "PIN must contain only printable ASCII characters".into(),
        ));
    }
    if estimate_pin_entropy_bits(pin) < MIN_PIN_ENTROPY_BITS {
        return Err(CryptoError::Validation(
            "PIN is too weak — use more characters or mix digits, letters, and symbols".into(),
        ));
    }
    Ok(())
}

/// Derive a 32-byte secret from a PIN and a salt (username) using Argon2id.
///
/// # Salt padding
///
/// Argon2id requires at least 8 bytes of salt. For usernames shorter than 8
/// bytes (after `trim().to_lowercase()` normalisation) we right-pad with NUL
/// bytes (`0x00`) up to the 8-byte minimum. This is safe and matches Signal's
/// reference behaviour for short identifiers because:
///
/// 1. **Argon2 hashes the salt internally.** The KDF compresses the salt
///    through BLAKE2b before mixing it into the memory-hard core, so the salt
///    contributes through its hash digest rather than its raw bit-length.
///    Adding deterministic NUL padding does not reduce the entropy of the
///    underlying username — the username is still globally unique, so the
///    derived hash input is still globally unique.
/// 2. **Padding is deterministic.** A given normalised username always pads to
///    the same byte string, so the same `(pin, username)` pair always yields
///    the same secret. This is required for SVR re-registration / recovery.
/// 3. **No collision risk in practice.** Usernames are constrained to a
///    server-enforced charset that excludes NUL, so a padded short username
///    `"ab\0\0\0\0\0\0"` cannot collide with any legitimate longer username.
///
/// Supported username length: any length `>= 1` byte after normalisation.
/// Lengths in `[1, 7]` are NUL-padded to 8 bytes; lengths `>= 8` are passed
/// through unchanged. Empty strings still pad to 8 NULs but should be
/// rejected at the caller (an empty username has no entropy as a salt).
pub fn derive_svr_secret(pin: &str, salt: &str) -> Result<[u8; 32], CryptoError> {
    let argon2 = Argon2::new(
        argon2::Algorithm::Argon2id,
        argon2::Version::V0x13,
        get_argon2_params(),
    );

    let mut output = [0u8; 32];

    // Normalize salt (username) for consistency
    let normalized_salt = salt.trim().to_lowercase();

    // Argon2id requires at least 8 bytes of salt, so we pad it if necessary.
    // See the function-level doc comment above for why NUL-padding is safe.
    let mut salt_bytes = normalized_salt.into_bytes();
    if salt_bytes.len() < 8 {
        salt_bytes.resize(8, 0);
    }
    
    argon2.hash_password_into(
        pin.as_bytes(),
        &salt_bytes,
        &mut output,
    ).map_err(|e| CryptoError::KeyDerivationFailed(e.to_string()))?;
    
    Ok(output)
}

/// Derive a local-only PIN fingerprint, used for in-memory PIN re-auth gates
/// (e.g. authorising a new linked device via PIN instead of password). Never
/// sent to the server. Computed as
/// `HMAC-SHA256(svr_secret, "Chatter Local PIN Fingerprint")`.
///
/// Persist with [`SettingsRepository::set_bytes(keys::PIN_FINGERPRINT, …)`]
/// on register / change-pin; constant-time compare on verify.
pub fn derive_pin_fingerprint(pin: &str, username: &str) -> Result<[u8; 32], CryptoError> {
    let mut svr_secret = derive_svr_secret(pin, username)?;
    type HmacSha256 = Hmac<Sha256>;
    let mut mac = HmacSha256::new_from_slice(&svr_secret)
        .expect("HMAC can take key of any size");
    mac.update(b"Chatter Local PIN Fingerprint");
    let result = mac.finalize();
    svr_secret.zeroize();
    let mut fp = [0u8; 32];
    fp.copy_from_slice(&result.into_bytes());
    Ok(fp)
}

/// Derive registration lock token from PIN.
/// Signal: HMAC-SHA256(svr_secret, "Registration Lock")
/// Server stores the hash; future re-registrations must present matching token.
pub fn derive_registration_lock(pin: &str, username: &str) -> Result<[u8; 32], CryptoError> {
    let mut svr_secret = derive_svr_secret(pin, username)?;
    type HmacSha256 = Hmac<Sha256>;
    let mut mac = HmacSha256::new_from_slice(&svr_secret)
        .expect("HMAC can take key of any size");
    mac.update(b"Registration Lock");
    let result = mac.finalize();

    // SECURITY: Zeroize intermediate secret immediately after HMAC computation
    svr_secret.zeroize();

    let mut lock = [0u8; 32];
    lock.copy_from_slice(&result.into_bytes());
    Ok(lock)
}

/// SVR Keypair for Secure Value Recovery.
///
/// # Security: Zeroize on Drop
///
/// The `secret` field (`StaticSecret` from x25519-dalek) implements `Zeroize` but does NOT
/// implement `ZeroizeOnDrop`. We explicitly implement `Drop` to ensure the private key material
/// is zeroized when the keypair goes out of scope, preventing key leakage through memory.
/// The `zeroize` feature is enabled on `x25519-dalek` in Cargo.toml to support this.
pub struct SvrKeypair {
    secret: StaticSecret,
    public: PublicKey,
}

impl Drop for SvrKeypair {
    fn drop(&mut self) {
        self.secret.zeroize();
    }
}

impl SvrKeypair {
    /// Generate an SVR keypair from a PIN and username (used as salt).
    pub fn new_from_pin(pin: &str, username: &str) -> Result<Self, CryptoError> {
        let mut secret_bytes = derive_svr_secret(pin, username)?;
        
        let secret = StaticSecret::from(secret_bytes);
        let public = PublicKey::from(&secret);
        
        secret_bytes.zeroize();
        
        Ok(Self { secret, public })
    }

    /// Get the public key bytes.
    pub fn public_key_bytes(&self) -> [u8; 32] {
        self.public.to_bytes()
    }
    
    /// Borrow the static secret (private key) bytes for the duration of `f`.
    ///
    /// # Why a closure?
    ///
    /// Returning `[u8; 32]` would copy the secret into a fresh stack/heap
    /// allocation that does **not** zeroize on drop, leaving plaintext key
    /// material lingering in memory past the caller's intent. Borrowing
    /// through a closure keeps the secret confined to the `StaticSecret`
    /// (which is zeroized in `Drop for SvrKeypair`) and forces every caller
    /// to use the bytes within a bounded scope.
    ///
    /// The temporary `[u8; 32]` returned by `StaticSecret::to_bytes()` is
    /// zeroized before this function returns.
    pub fn with_secret_bytes<R>(&self, f: impl FnOnce(&[u8; 32]) -> R) -> R {
        let mut bytes = self.secret.to_bytes();
        let result = f(&bytes);
        bytes.zeroize();
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Padding short usernames must be deterministic: the same username always
    /// derives the same secret regardless of how many NUL bytes were appended.
    /// This guards the salt-padding rationale documented on `derive_svr_secret`.
    #[test]
    fn salt_padding_is_deterministic_for_short_usernames() {
        // 3-byte username — gets padded to 8 bytes with 5 NULs
        let a1 = derive_svr_secret("123456", "abc").unwrap();
        let a2 = derive_svr_secret("123456", "abc").unwrap();
        assert_eq!(a1, a2, "same short username must derive the same secret");

        // 7-byte username — gets padded to 8 bytes with 1 NUL
        let b1 = derive_svr_secret("123456", "shorty7").unwrap();
        let b2 = derive_svr_secret("123456", "shorty7").unwrap();
        assert_eq!(b1, b2, "7-byte username must derive deterministically");

        // 8-byte username — no padding applied
        let c1 = derive_svr_secret("123456", "exactly8").unwrap();
        let c2 = derive_svr_secret("123456", "exactly8").unwrap();
        assert_eq!(c1, c2, "8-byte username must derive deterministically");

        // 32-byte username — no padding applied
        let long = "abcdefghijklmnopqrstuvwxyz012345"; // 32 bytes
        let d1 = derive_svr_secret("123456", long).unwrap();
        let d2 = derive_svr_secret("123456", long).unwrap();
        assert_eq!(d1, d2, "long username must derive deterministically");

        // Different lengths must yield different secrets — padding does NOT
        // make a 3-byte username collide with a longer one or with raw NULs.
        assert_ne!(a1, b1);
        assert_ne!(a1, c1);
        assert_ne!(b1, c1);
        assert_ne!(c1, d1);
    }

    /// `with_secret_bytes` must yield the same bytes for the same `(pin, username)`
    /// pair across independent keypair instances (deterministic derivation).
    #[test]
    fn with_secret_bytes_is_deterministic() {
        let kp1 = SvrKeypair::new_from_pin("123456", "alice").unwrap();
        let kp2 = SvrKeypair::new_from_pin("123456", "alice").unwrap();

        let eq = kp1.with_secret_bytes(|a| kp2.with_secret_bytes(|b| a == b));
        assert!(eq, "same (pin, username) must produce the same secret bytes");
    }

    #[test]
    fn validate_pin_rejects_too_short() {
        // 6-digit PINs were the pre-fix default - rejected now without an
        // enclave to throttle attempts. See SECURITY.md "SVR enclave gap".
        for short in &["", "1", "12", "1234567", "abc"] {
            assert!(
                validate_pin(short).is_err(),
                "{short:?} should be rejected as too short"
            );
        }
    }

    #[test]
    fn validate_pin_rejects_too_long() {
        let too_long = "a".repeat(MAX_PIN_LENGTH + 1);
        assert!(validate_pin(&too_long).is_err());
    }

    #[test]
    fn validate_pin_accepts_minimum_length() {
        assert!(validate_pin("abcdefgh").is_ok(), "8 lowercase (~37.6 bits) accepted");
        assert!(validate_pin("AaBbCc12").is_ok(), "mixed alphanumeric accepted");
        assert!(validate_pin("Pa$$w0rd!").is_ok(), "symbols accepted");
    }

    #[test]
    fn validate_pin_rejects_low_entropy() {
        // 8-digit numeric (~26.6 bits) is below the entropy floor — the case a
        // software-only SVR can't defend against.
        assert!(validate_pin("12345678").is_err(), "8-digit numeric rejected");
        // A longer numeric PIN clears the floor (10 digits ~= 33 bits).
        assert!(validate_pin("1234567890").is_ok(), "10-digit numeric accepted");
    }

    #[test]
    fn validate_pin_rejects_non_printable() {
        // Tab, newline, control chars all rejected.
        assert!(validate_pin("abcdef\tgh").is_err());
        assert!(validate_pin("abcdef\ngh").is_err());
        assert!(validate_pin("abcdef\x00gh").is_err());
    }

    #[test]
    fn validate_pin_rejects_non_ascii() {
        // Unicode rejected - keeps PIN bytes deterministic across locales.
        assert!(validate_pin("hunter2é").is_err());
    }

    /// Argon2 params changed from `(64 MiB, 3, 4)` to `(128 MiB, 8, 4)`. This
    /// test pins the new parameters so a future drop back to the old numbers
    /// without enclave protection fails CI loudly.
    #[test]
    fn argon2_params_match_no_enclave_baseline() {
        let p = get_argon2_params();
        assert_eq!(p.m_cost(), 131072, "expected 128 MiB memory cost");
        assert_eq!(p.t_cost(), 8, "expected 8 iterations");
        assert_eq!(p.p_cost(), 4, "expected 4 lanes");
        assert_eq!(p.output_len(), Some(32));
    }
}

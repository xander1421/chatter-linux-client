//! Secure Value Recovery (SVR) cryptography.
//!
//! Handles derivation of the SVR keypair from a user-provided PIN.
//! Uses Argon2id to provide memory-hard derivation, protecting against brute-force
//! attacks on short PINs. The username is used as a deterministic salt.

use argon2::{Argon2, Params};
use hkdf::Hkdf;
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
pub(crate) fn get_argon2_params() -> Params {
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
    if pin
        .chars()
        .any(|c| c.is_ascii_graphic() && !c.is_ascii_alphanumeric())
    {
        charset += 32;
    }
    if charset == 0 {
        return 0.0;
    }
    pin.chars().count() as f64 * (charset as f64).log2()
}

/// The most common passwords of [`MIN_PIN_LENGTH`]+ characters, from the
/// public breach-corpus top lists (rockyou / NCSC / SecLists top-10k,
/// filtered to >= 8 chars). Matched case-insensitively.
///
/// This exists because the entropy estimate above is a CHARSET model:
/// "password" scores 8 x log2(26) = 37.6 "bits" and sails over the 30-bit
/// floor, while sitting in the first hundred guesses of every cracking
/// wordlist. A curated list of the head of that distribution costs nothing
/// and removes exactly the guesses an offline attacker tries first
/// (OPEN-ITEMS §10). It deliberately stays small — the tail adds little,
/// because past the head the structural checks and the entropy floor carry
/// the weight.
const BLOCKED_PINS: &[&str] = &[
    "password",
    "password1",
    "password12",
    "password123",
    "passw0rd",
    "p@ssw0rd",
    "12345678",
    "123456789",
    "1234567890",
    "123123123",
    "987654321",
    "11111111",
    "00000000",
    "1q2w3e4r",
    "1q2w3e4r5t",
    "1qaz2wsx",
    "qazwsx123",
    "aa123456",
    "abc12345",
    "abcd1234",
    "qwertyui",
    "qwertyuiop",
    "asdfghjk",
    "asdfghjkl",
    "zxcvbnm123",
    "iloveyou",
    "iloveyou1",
    "sunshine",
    "princess",
    "football",
    "baseball",
    "superman",
    "trustno1",
    "welcome1",
    "michelle",
    "jennifer",
    "computer",
    "whatever",
    "chocolate",
    "butterfly",
    "liverpool",
    "starwars",
    "pokemon1",
    "michael1",
    "jordan23",
    "harley123",
    "shadow123",
    "monkey123",
    "dragon123",
    "master123",
    "samantha",
    "anthony1",
    "danielle",
    "jessica1",
    "charlie1",
    "internet",
    "creative",
    "adventure",
    "elephant",
    "November",
    "December",
    "September",
    "alexander",
    "victoria",
    "kimberly",
    "veronica",
    "corvette",
    "mercedes",
    "midnight",
    "snickers",
    "mountain",
    "dinosaur",
    "hardcore",
    "lovelove",
    "loveyou2",
    "babygirl",
    "babygirl1",
    "angelina",
    "valentina",
    "isabella",
    "gabriella",
    "benjamin",
    "nicholas",
    "matthew1",
];

/// Blocklist membership, case-insensitive.
fn is_blocklisted_pin(pin: &str) -> bool {
    let lowered = pin.to_ascii_lowercase();
    BLOCKED_PINS
        .iter()
        .any(|b| b.eq_ignore_ascii_case(&lowered))
}

/// Structural weakness: patterns whose keyspace is tiny regardless of what
/// the charset model says. Each catches a whole family no finite blocklist
/// can enumerate:
///
/// - all one character:            "aaaaaaaa", "!!!!!!!!"  (charset model: 37.6 bits)
/// - a single +/-1 run:            "abcdefgh", "87654321"
/// - a short block repeated:       "12341234", "abcabcab", "xyxyxyxy"
///
/// The period check accepts any period `p <= len/2` with `s[i] == s[i-p]`,
/// which also catches non-integer tilings like "abcabcab" (period 3 over 8).
fn is_structurally_weak_pin(pin: &str) -> bool {
    let b = pin.as_bytes();
    // All one character.
    if b.iter().all(|&c| c == b[0]) {
        return true;
    }
    // Single monotone run with step +1 or -1 ("12345678", "hgfedcba").
    for step in [1i16, -1i16] {
        if b.windows(2).all(|w| w[1] as i16 - w[0] as i16 == step) {
            return true;
        }
    }
    // Repeated short block.
    for period in 1..=b.len() / 2 {
        if (period..b.len()).all(|i| b[i] == b[i - period]) {
            return true;
        }
    }
    false
}

/// Validate a PIN's shape and strength.
///
/// `MIN_PIN_LENGTH..=MAX_PIN_LENGTH` ASCII printable bytes, at least
/// [`MIN_PIN_ENTROPY_BITS`] of estimated entropy, not in the common-password
/// blocklist, and not structurally degenerate (one repeated character, a
/// keyboard-walk run, a short repeated block). The entropy floor rejects
/// low-keyspace PINs (e.g. an 8-digit numeric PIN); the blocklist and the
/// structural checks reject the high-scoring-but-first-guessed PINs the
/// charset model cannot see — "password" scores 37.6 estimated bits and is
/// the first line of every cracking wordlist (OPEN-ITEMS §10).
pub fn validate_pin(pin: &str) -> Result<(), CryptoError> {
    validate_pin_shape(pin)?;
    if estimate_pin_entropy_bits(pin) < MIN_PIN_ENTROPY_BITS {
        return Err(CryptoError::Validation(
            "PIN is too weak — use more characters or mix digits, letters, and symbols".into(),
        ));
    }
    if is_blocklisted_pin(pin) || is_structurally_weak_pin(pin) {
        return Err(CryptoError::Validation(
            "PIN is too common or too predictable — choose something less guessable".into(),
        ));
    }
    Ok(())
}

/// Shape-only validation: length bounds and printable ASCII, WITHOUT the
/// entropy floor, blocklist or structural checks.
///
/// This is the check for paths that USE an already-set PIN — verifying it,
/// restoring a backup with it, re-uploading with it. Those paths must accept
/// every PIN that was valid when it was SET: the strength rules only ratchet
/// forward (the §10 blocklist grew once and will again), and a rule added
/// after a user chose their PIN must never stand between them and their own
/// backup. Strength enforcement belongs exclusively to the creation paths
/// (registration, change-pin, new backup passphrase), which use
/// [`validate_pin`].
pub fn validate_pin_shape(pin: &str) -> Result<(), CryptoError> {
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

    argon2
        .hash_password_into(pin.as_bytes(), &salt_bytes, &mut output)
        .map_err(|e| CryptoError::KeyDerivationFailed(e.to_string()))?;

    Ok(output)
}

/// Async wrapper for [`derive_svr_secret`] that runs the KDF on the blocking
/// pool instead of the caller's async worker thread.
///
/// CORRECTNESS: [`derive_svr_secret`] burns ~450ms of uninterruptible CPU
/// (128 MiB, t=8). Called directly from an `async fn` it blocks the whole
/// tokio worker — and on a current-thread runtime (which is what
/// `#[tokio::test]` builds, and what a UI embedding may use) that is the ONLY
/// worker. Every other task on the runtime stops, including the HTTP/2
/// connection task that must read the server's SETTINGS frame and answer
/// pings. The connection then dies while the client is merely busy hashing,
/// surfacing as a spurious "server unavailable" / "timeout waiting for
/// SETTINGS frames" that looks exactly like server downtime. Always reach the
/// KDF through this from async code.
pub async fn derive_svr_secret_async(pin: &str, username: &str) -> Result<[u8; 32], CryptoError> {
    let pin = pin.to_owned();
    let username = username.to_owned();
    tokio::task::spawn_blocking(move || derive_svr_secret(&pin, &username))
        .await
        .map_err(|e| CryptoError::KeyDerivationFailed(format!("svr kdf task failed: {e}")))?
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
    let fp = pin_fingerprint_from_secret(&svr_secret);
    svr_secret.zeroize();
    Ok(fp)
}

/// Derive a purpose-specific 32-byte subkey from the SVR secret.
///
/// CRYPTO-3: the Argon2id output is one secret with several consumers — an
/// AES-256-GCM key, a lookup index the server sees, an X25519 static scalar and
/// two more authenticators. Handing the same 32 bytes to different primitives
/// gives none of them a key of their own, so a weakness or a leak in any one of
/// them is a weakness in all of them. Every consumer now takes an HKDF-SHA256
/// expansion under its own info string, exactly as `message_backup::
/// derive_server_kek` already did.
pub fn svr_subkey(svr_secret: &[u8; 32], info: &[u8]) -> [u8; 32] {
    let hk = Hkdf::<Sha256>::new(None, svr_secret);
    let mut out = [0u8; 32];
    hk.expand(info, &mut out)
        .expect("32 bytes is a valid HKDF-SHA256 output length");
    out
}

/// HKDF info strings. Each consumer of the SVR secret gets exactly one.
pub(crate) const SVR_INFO_PIN_FINGERPRINT: &[u8] = b"Chatter SVR Local PIN Fingerprint v1";
pub(crate) const SVR_INFO_REGISTRATION_LOCK: &[u8] = b"Chatter SVR Registration Lock v1";
pub(crate) const SVR_INFO_X25519: &[u8] = b"Chatter SVR X25519 Static Secret v1";

/// Same as [`derive_pin_fingerprint`] but takes an already-derived SVR secret.
///
/// PERF: [`derive_svr_secret`] is a deliberately expensive memory-hard KDF
/// (~450ms, 128 MiB — see [`get_argon2_params`]). A caller that already holds
/// the secret for `(pin, username)` must not pay for it again: the derivation
/// is deterministic, so recomputing it yields bit-identical output for zero
/// security benefit. Callers deriving several values from one PIN should call
/// [`derive_svr_secret`] once and use the `*_from_secret` variants.
pub fn pin_fingerprint_from_secret(svr_secret: &[u8; 32]) -> [u8; 32] {
    svr_subkey(svr_secret, SVR_INFO_PIN_FINGERPRINT)
}

/// Derive registration lock token from PIN.
/// Signal: HMAC-SHA256(svr_secret, "Registration Lock")
/// Server stores the hash; future re-registrations must present matching token.
pub fn derive_registration_lock(pin: &str, username: &str) -> Result<[u8; 32], CryptoError> {
    let mut svr_secret = derive_svr_secret(pin, username)?;
    let lock = registration_lock_from_secret(&svr_secret);

    // SECURITY: Zeroize intermediate secret immediately after HMAC computation
    svr_secret.zeroize();

    Ok(lock)
}

/// Same as [`derive_registration_lock`] but takes an already-derived SVR secret.
/// See [`pin_fingerprint_from_secret`] for why callers should prefer this.
pub fn registration_lock_from_secret(svr_secret: &[u8; 32]) -> [u8; 32] {
    svr_subkey(svr_secret, SVR_INFO_REGISTRATION_LOCK)
}

/// SVR Keypair for Secure Value Recovery.
///
/// # Security: Zeroize on Drop
///
/// Dalek 3's `StaticSecret` implements `Drop` and `ZeroizeOnDrop` with the
/// explicitly enabled `zeroize` feature, clearing the private key on release.
pub struct SvrKeypair {
    secret: StaticSecret,
    public: PublicKey,
}

impl SvrKeypair {
    /// Generate an SVR keypair from a PIN and username (used as salt).
    pub fn new_from_pin(pin: &str, username: &str) -> Result<Self, CryptoError> {
        let mut secret_bytes = derive_svr_secret(pin, username)?;
        let kp = Self::from_secret(&secret_bytes);
        secret_bytes.zeroize();
        Ok(kp)
    }

    /// Same as [`SvrKeypair::new_from_pin`] but takes an already-derived SVR
    /// secret. See [`pin_fingerprint_from_secret`] for why callers should
    /// prefer this.
    pub fn from_secret(svr_secret: &[u8; 32]) -> Self {
        // CRYPTO-3: an X25519 static scalar of its own, not the raw SVR secret
        // that also keys AES-GCM and the server-visible backup id.
        let mut scalar = svr_subkey(svr_secret, SVR_INFO_X25519);
        let secret = StaticSecret::from(scalar);
        scalar.zeroize();
        let public = PublicKey::from(&secret);
        Self { secret, public }
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

    /// The §10 defect, pinned: PINs that pass the charset entropy model while
    /// sitting at the head of every cracking wordlist. Every one of these
    /// scored >= 30 estimated bits under the old rules and was ACCEPTED.
    #[test]
    fn common_and_predictable_pins_are_rejected_despite_high_charset_entropy() {
        for pin in [
            "password",   // 37.6 "bits", first line of rockyou (§10's own example)
            "Password1",  // blocklist is case-insensitive
            "iloveyou",   // common-list
            "aaaaaaaa",   // one repeated char — 37.6 "bits" under the charset model
            "abcdefgh",   // +1 run
            "hgfedcba",   // -1 run
            "12341234",   // repeated block, integer tiling
            "abcabcab",   // repeated block, NON-integer tiling (period 3 over 8)
            "xyxyxyxyxy", // period 2
        ] {
            assert!(
                validate_pin(pin).is_err(),
                "{pin:?} must be rejected — it is among an offline attacker's first guesses"
            );
        }
    }

    /// Reasonable PINs keep passing — including the VALIDATED harness PIN, so
    /// this check can never lock the integration suite out.
    #[test]
    fn reasonable_pins_still_pass() {
        for pin in [
            "Va1idPin!x9",
            "tr0mb0ne-cloud",
            "Bl4ckb1rd$",
            "novemberrain77",
        ] {
            assert!(
                validate_pin(pin).is_ok(),
                "{pin:?} should pass but was rejected"
            );
        }
    }

    /// The ratchet-forward invariant, load-bearing for account recovery: a PIN
    /// that is now blocklisted or structurally weak must still pass the
    /// SHAPE-only check, because that is what the login, SVR-restore, backup
    /// and change-old-PIN paths run. If strict validation ever leaks onto one
    /// of those paths, a user whose PIN predates a blocklist entry is locked
    /// out of their own account and their own backup — the regression cubic
    /// caught on the login path (§10).
    #[test]
    fn a_now_weak_pin_still_passes_shape_only_so_its_owner_can_still_log_in() {
        for pin in ["password", "abcd1234", "12345678", "aaaaaaaa", "abcdefgh"] {
            assert!(
                validate_pin(pin).is_err(),
                "{pin:?} must be refused when a PIN is being CHOSEN"
            );
            assert!(
                validate_pin_shape(pin).is_ok(),
                "{pin:?} must pass shape-only — its owner set it when it was valid and \
                 must still be able to log in and restore",
            );
        }
    }

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
        assert!(
            eq,
            "same (pin, username) must produce the same secret bytes"
        );
    }

    /// The `*_from_secret` variants exist so the registration/login paths can
    /// pay for the 128 MiB Argon2id ONCE instead of once per derived value.
    /// That is only sound if they are bit-identical to the `pin`-taking
    /// originals. If this ever fails, the "derive once and reuse" callers in
    /// `auth_service` are silently writing different key material than the
    /// functions they replaced — so pin the equivalence here.
    #[test]
    fn from_secret_variants_match_the_pin_variants_exactly() {
        let (pin, user) = ("12345678", "equivalence_user");
        let secret = derive_svr_secret(pin, user).unwrap();

        assert_eq!(
            registration_lock_from_secret(&secret),
            derive_registration_lock(pin, user).unwrap(),
            "registration_lock_from_secret must equal derive_registration_lock"
        );
        assert_eq!(
            pin_fingerprint_from_secret(&secret),
            derive_pin_fingerprint(pin, user).unwrap(),
            "pin_fingerprint_from_secret must equal derive_pin_fingerprint"
        );
        assert_eq!(
            SvrKeypair::from_secret(&secret).public_key_bytes(),
            SvrKeypair::new_from_pin(pin, user)
                .unwrap()
                .public_key_bytes(),
            "SvrKeypair::from_secret must equal SvrKeypair::new_from_pin"
        );
    }

    /// The async wrapper must not change the derived value — it only moves the
    /// same computation onto the blocking pool.
    #[tokio::test]
    async fn derive_svr_secret_async_matches_sync() {
        let (pin, user) = ("12345678", "async_user");
        assert_eq!(
            derive_svr_secret_async(pin, user).await.unwrap(),
            derive_svr_secret(pin, user).unwrap(),
            "async wrapper must produce the same secret as the sync KDF"
        );
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
        // NOT "abcdefgh" — that is a keyboard-walk run the structural check
        // now rejects (§10); an 8-char PIN must be fine when it isn't a
        // pattern.
        assert!(
            validate_pin("kf8mq2xp").is_ok(),
            "8 mixed lowercase+digits accepted"
        );
        assert!(
            validate_pin("AaBbCc12").is_ok(),
            "mixed alphanumeric accepted"
        );
        assert!(validate_pin("Pa$$w0rd!").is_ok(), "symbols accepted");
    }

    #[test]
    fn validate_pin_rejects_low_entropy() {
        // 8-digit numeric (~26.6 bits) is below the entropy floor — the case a
        // software-only SVR can't defend against.
        assert!(
            validate_pin("12345678").is_err(),
            "8-digit numeric rejected"
        );
        // A longer numeric PIN clears the entropy floor (10 digits ~= 33
        // bits), but "1234567890" specifically is a +1 run AND on the
        // blocklist — the §10 case of a high-scoring first-guess. A
        // non-pattern 10-digit PIN passes.
        assert!(
            validate_pin("1234567890").is_err(),
            "the canonical 10-digit run rejected"
        );
        assert!(
            validate_pin("8629174035").is_ok(),
            "non-pattern 10-digit numeric accepted"
        );
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

    /// CRYPTO-3: the Argon2id SVR secret used to be handed unmodified to an
    /// AES-256-GCM key, an HMAC key whose output the server sees as a lookup
    /// index, an X25519 static scalar and two more HMAC keys — one secret, five
    /// primitives, no separation. Each consumer now takes its own HKDF-SHA256
    /// expansion. These vectors were computed independently (Python
    /// hmac/hashlib HKDF over ikm = 0x00..0x1f) so a silent change to an info
    /// string — which re-keys every backup and every registration lock — fails
    /// loudly instead.
    #[test]
    fn svr_subkeys_match_pinned_vectors_and_are_distinct() {
        let secret: [u8; 32] = core::array::from_fn(|i| i as u8);

        assert_eq!(
            hex::encode(pin_fingerprint_from_secret(&secret)),
            "1ca0b8c4ab8fc1732fd3dc28680ef40168d18cb1b50d4e6146da26a3ff8d05a9",
        );
        assert_eq!(
            hex::encode(registration_lock_from_secret(&secret)),
            "fec745f7e2513c68282a8dceb96077b0a57405e85f2f237faecef53f657ad002",
        );
        assert_eq!(
            hex::encode(svr_subkey(&secret, SVR_INFO_X25519)),
            "fae4444170eda3a0b0b22c8108d6dcb5e76ce24a711c879d3175151ca4a6b337",
        );

        // No consumer may see the raw secret, and no two may see the same bytes.
        let subkeys = [
            pin_fingerprint_from_secret(&secret),
            registration_lock_from_secret(&secret),
            svr_subkey(&secret, SVR_INFO_X25519),
            crate::crypto::svr_backup::derive_backup_id(&secret),
        ];
        for (i, a) in subkeys.iter().enumerate() {
            assert_ne!(*a, secret, "subkey {i} is the raw SVR secret");
            for b in subkeys.iter().skip(i + 1) {
                assert_ne!(a, b, "two consumers share key material");
            }
        }

        // The X25519 scalar is derived, not the secret itself.
        assert_ne!(SvrKeypair::from_secret(&secret).public_key_bytes(), {
            use x25519_dalek::{PublicKey, StaticSecret};
            PublicKey::from(&StaticSecret::from(secret)).to_bytes()
        });
    }
}

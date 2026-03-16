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

/// The parameters for Argon2id hashing for PIN derivation.
/// These should provide a good balance between memory hardness and computation time.
fn get_argon2_params() -> Params {
    Params::new(
        65536, // m_cost (64 MB)
        3,     // t_cost (3 iterations)
        4,     // p_cost (4 lanes)
        Some(32), // output length (32 bytes)
    ).expect("Valid Argon2 parameters")
}

/// Derive a 32-byte secret from a PIN and a salt (username) using Argon2id.
pub fn derive_svr_secret(pin: &str, salt: &str) -> Result<[u8; 32], CryptoError> {
    let argon2 = Argon2::new(
        argon2::Algorithm::Argon2id,
        argon2::Version::V0x13,
        get_argon2_params(),
    );

    let mut output = [0u8; 32];
    
    // Normalize salt (username) for consistency
    let normalized_salt = salt.trim().to_lowercase();
    
    // Argon2id requires at least 8 bytes of salt, so we pad it if necessary
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

/// Derive registration lock token from PIN.
/// Signal: HMAC-SHA256(svr_secret, "Registration Lock")
/// Server stores the hash; future re-registrations must present matching token.
pub fn derive_registration_lock(pin: &str, username: &str) -> Result<[u8; 32], CryptoError> {
    let svr_secret = derive_svr_secret(pin, username)?;
    type HmacSha256 = Hmac<Sha256>;
    let mut mac = HmacSha256::new_from_slice(&svr_secret)
        .expect("HMAC can take key of any size");
    mac.update(b"Registration Lock");
    let result = mac.finalize();
    let mut lock = [0u8; 32];
    lock.copy_from_slice(&result.into_bytes());
    Ok(lock)
}

/// SVR Keypair for Secure Value Recovery.
pub struct SvrKeypair {
    secret: StaticSecret,
    public: PublicKey,
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
    
    /// Get the static secret (private key) bytes for encryption/decryption tasks
    pub fn secret_bytes(&self) -> [u8; 32] {
        self.secret.to_bytes()
    }
}

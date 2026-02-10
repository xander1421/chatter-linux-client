//! RSA blind signatures for ephemeral mailbox tokens.
//!
//! Implements Chaum's RSA blind signature scheme for anonymous mailbox creation.
//! Based on RFC 9474 and NDSS '21 "Improving Signal's Sealed Sender" proposal.

use blind_rsa_signatures::{BlindSignature as BlindSig, Options, PublicKey, Signature};
use rsa::pkcs1::DecodeRsaPublicKey;

use crate::error::CryptoError;

/// Blind signature manager for ephemeral mailbox tokens.
///
/// Provides RSA blind signatures to anonymously prove authorization
/// to create ephemeral mailboxes without revealing the specific mailbox key.
/// Wrapper for blind RSA signature operations using RFC 9474 compliant mode.
/// Uses RSABSSA-SHA384-PSS-Randomized for production security.
#[derive(Clone)]
pub struct BlindSignature {
    /// Server's RSA-2048 public key (for blinding)
    pub server_public_key: PublicKey,
    /// Blind signature options
    pub options: Options,
}

/// Blinding result containing blinded message and secret.
///
/// The secret must be kept confidential for later unblinding.
pub type BlindingResult = blind_rsa_signatures::BlindingResult;

impl BlindSignature {
    /// Create a new BlindSignature manager with the server's public key.
    ///
    /// # Arguments
    ///
    /// * `server_pubkey_der` - Server's RSA-2048 public key in DER format
    pub fn new(server_pubkey_der: &[u8]) -> Result<Self, CryptoError> {
        // Parse DER-encoded RSA public key
        let rsa_pubkey = rsa::RsaPublicKey::from_pkcs1_der(server_pubkey_der)
            .map_err(|e| CryptoError::InvalidKey(format!("Invalid RSA public key: {}", e)))?;

        // Convert to blind-rsa-signatures PublicKey
        let server_public_key = PublicKey::new(rsa_pubkey);

        // Use RSABSSA-SHA384-PSS-Randomized variant per RFC 9474 recommendation
        // This variant uses PSS with salt and message randomization for security
        let options = Options::new(
            blind_rsa_signatures::Hash::Sha384,
            false,  // randomized PSS padding (with salt)
            48,     // salt_len = SHA-384 output size (48 bytes)
        );

        Ok(Self {
            server_public_key,
            options,
        })
    }

    /// Blind a message (ephemeral public key) for signing.
    ///
    /// Returns a BlindingResult containing the blinded message and secret.
    /// The secret must be kept for later unblinding.
    ///
    /// # Arguments
    ///
    /// * `message` - The message to blind (typically an Ed25519 public key)
    pub fn blind(&self, message: &[u8]) -> Result<BlindingResult, CryptoError> {
        // Use randomized message preparation (PrepareRandomize)
        // This prepends a 32-byte random prefix for security against malicious signers
        // RFC 9474 Section 7.3: protects against low-entropy message attacks
        self.server_public_key
            .blind(message, true, &self.options)  // randomize_message = true
            .map_err(|e| CryptoError::BlindSignatureError(format!("Blinding failed: {}", e)))
    }

    /// Unblind a signature received from the server.
    ///
    /// # Arguments
    ///
    /// * `blinded_sig` - The blind signature from the server
    /// * `blinding_result` - The blinding result from the blind operation
    /// * `message` - The original message that was blinded
    pub fn unblind(
        &self,
        blinded_sig: &[u8],
        blinding_result: &BlindingResult,
        message: &[u8],
    ) -> Result<Vec<u8>, CryptoError> {
        // Convert slice to BlindSignature type
        let blind_sig = BlindSig::from(blinded_sig.to_vec());

        // Finalize (unblind) the signature using the public key
        let signature = self
            .server_public_key
            .finalize(
                &blind_sig,
                &blinding_result.secret,
                blinding_result.msg_randomizer,
                message,
                &self.options,
            )
            .map_err(|e| CryptoError::BlindSignatureError(format!("Unblinding failed: {}", e)))?;

        Ok(signature.to_vec())
    }

    /// Verify an unblinded signature with message randomizer.
    ///
    /// Verifies that the signature is valid for the given message.
    /// When the message was blinded with `randomize_message=true`, the randomizer
    /// must be provided for correct verification.
    ///
    /// # Arguments
    ///
    /// * `message` - The original message that was blinded
    /// * `signature` - The unblinded signature
    /// * `msg_randomizer` - Optional 32-byte message randomizer (required if randomize_message=true)
    pub fn verify(&self, message: &[u8], signature: &[u8], msg_randomizer: Option<&[u8]>) -> bool {
        // Convert slice to Signature type
        let sig = Signature::from(signature.to_vec());

        // Convert randomizer if provided (must be exactly 32 bytes)
        let randomizer = msg_randomizer.and_then(|r| {
            if r.len() == 32 {
                let mut arr = [0u8; 32];
                arr.copy_from_slice(r);
                Some(blind_rsa_signatures::MessageRandomizer::from(arr))
            } else {
                None
            }
        });

        // Verify signature
        self.server_public_key
            .verify(&sig, randomizer, message, &self.options)
            .is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use blind_rsa_signatures::{KeyPair, Options};
    use rsa::PublicKeyParts;

    #[test]
    fn test_blind_unblind_verify_flow() {
        // Generate a test RSA-2048 keypair (in production, server provides public key)
        let kp = KeyPair::generate(2048).unwrap();
        
        // Export public key - use the modulus bytes as a simple DER representation
        // In production, server would provide proper PKCS#1 DER format
        let pubkey = &kp.pk;
        let n_bytes = pubkey.as_ref().n().to_bytes_be();
        
        // For testing, use randomized variant matching production
        let blind_sig = BlindSignature {
            server_public_key: kp.pk.clone(),
            options: Options::new(
                blind_rsa_signatures::Hash::Sha384,
                false,  // randomized PSS
                48,     // salt_len
            ),
        };

        // Message to blind (e.g., an Ed25519 public key)
        let message = b"test ephemeral public key 12345678901234";

        // 1. Client: Blind the message
        let blinding_result = blind_sig.blind(message).unwrap();
        let blinded_msg: &[u8] = blinding_result.blind_msg.as_ref();

        // 2. Server: Sign the blinded message (simulated)
        let blinded_signature = kp
            .sk
            .blind_sign(
                blinded_msg,
                &Options::new(
                    blind_rsa_signatures::Hash::Sha384,
                    false,  // randomized PSS
                    48,     // salt_len
                ),
            )
            .unwrap();

        // 3. Client: Unblind the signature
        let signature = blind_sig
            .unblind(blinded_signature.as_ref(), &blinding_result, message)
            .unwrap();

        // 4. Verify the signature (need to pass randomizer since we used randomize_message=true)
        let randomizer: Option<&[u8]> = blinding_result.msg_randomizer.as_ref().map(|r| r.as_slice());
        assert!(blind_sig.verify(message, &signature, randomizer));
    }

    #[test]
    fn test_verify_invalid_signature() {
        let kp = KeyPair::generate(2048).unwrap();
        let blind_sig = BlindSignature {
            server_public_key: kp.pk.clone(),
            options: Options::new(
                blind_rsa_signatures::Hash::Sha384,
                false,  // randomized PSS
                48,     // salt_len
            ),
        };

        let message = b"test message";
        let wrong_signature = vec![0u8; 256];

        assert!(!blind_sig.verify(message, &wrong_signature, None));
    }
}

//! Encrypted device names — a port of Signal-Desktop `ts/Crypto.node.ts`
//! `encryptDeviceName` / `decryptDeviceName`, wire-encoded as Signal's
//! `DeviceName` proto (`protos/DeviceName.proto`; ours is `chat.v1.DeviceName`
//! in auth.proto).
//!
//! The name is encrypted to the ACCOUNT's identity public key, so only the
//! account's own devices (which all hold the identity key pair) can read it;
//! the server stores the encoded bytes opaquely.
//!
//! ```text
//! master       = X25519(ephemeral_private, identity_public)
//! synthetic_iv = HMAC-SHA256(HMAC-SHA256(master, "auth"), plaintext)[..16]
//! cipher_key   = HMAC-SHA256(HMAC-SHA256(master, "cipher"), synthetic_iv)
//! ciphertext   = AES-256-CTR(cipher_key, counter = 0^16, plaintext)
//! ```
//!
//! Decryption recomputes the synthetic IV from the recovered plaintext and
//! compares it in constant time; that comparison is the only integrity check
//! (SIV construction), exactly as in Signal-Desktop.
//!
//! The DH runs inside libsignal (`PrivateKey::calculate_agreement`); only the
//! 32-byte agreement output leaves it, held in `Zeroizing` like every key
//! derived from it.

use aes::cipher::{KeyIvInit, StreamCipher};
use hmac::{Hmac, Mac};
use libsignal_protocol::{IdentityKey, IdentityKeyPair, KeyPair, PublicKey};
use prost::Message;
use sha2::Sha256;
use subtle::ConstantTimeEq;
use zeroize::Zeroizing;

use crate::error::CryptoError;
use crate::proto::chat_v1::DeviceName;

type Aes256Ctr = ctr::Ctr128BE<aes::Aes256>;

/// Upper bound on the encoded `DeviceName` — Signal-Server's
/// `SetDeviceNameRequest.name` bound (device.proto, 1..225), which our server
/// and the `encrypted_name` buf.validate rules enforce too. With a 33-byte key
/// and a 16-byte IV the encoding leaves 169 bytes of UTF-8 for the name.
pub const MAX_ENCODED_DEVICE_NAME_LEN: usize = 225;

const SYNTHETIC_IV_LEN: usize = 16;

fn hmac_sha256(key: &[u8], data: &[u8]) -> Zeroizing<[u8; 32]> {
    let mut mac =
        <Hmac<Sha256> as hmac::KeyInit>::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(data);
    Zeroizing::new(mac.finalize().into_bytes().into())
}

fn agreement(
    private: &libsignal_protocol::PrivateKey,
    public: &PublicKey,
) -> Result<Zeroizing<Vec<u8>>, CryptoError> {
    private
        .calculate_agreement(public)
        .map(|s| Zeroizing::new(s.into_vec()))
        .map_err(|e| CryptoError::Protocol(format!("device name agreement: {e}")))
}

fn synthetic_iv(master: &[u8], plaintext: &[u8]) -> [u8; SYNTHETIC_IV_LEN] {
    let key1 = hmac_sha256(master, b"auth");
    let mac = hmac_sha256(&*key1, plaintext);
    let mut iv = [0u8; SYNTHETIC_IV_LEN];
    iv.copy_from_slice(&mac[..SYNTHETIC_IV_LEN]);
    iv
}

fn apply_ctr(master: &[u8], synthetic_iv: &[u8], buf: &mut [u8]) {
    let key2 = hmac_sha256(master, b"cipher");
    let cipher_key = hmac_sha256(&*key2, synthetic_iv);
    let mut cipher = Aes256Ctr::new((&*cipher_key).into(), &[0u8; 16].into());
    cipher.apply_keystream(buf);
}

/// Encrypt `name` to the account identity key and return the serialized
/// `DeviceName` the server stores. Errors when the encoding would exceed
/// [`MAX_ENCODED_DEVICE_NAME_LEN`] (the server would reject it).
pub fn encrypt_device_name(
    name: &str,
    account_identity: &IdentityKey,
) -> Result<Vec<u8>, CryptoError> {
    use rand::TryRngCore as _;
    let ephemeral = KeyPair::generate(&mut rand::rngs::OsRng.unwrap_err());
    encrypt_with_ephemeral(name, account_identity.public_key(), &ephemeral)
}

fn encrypt_with_ephemeral(
    name: &str,
    identity_public: &PublicKey,
    ephemeral: &KeyPair,
) -> Result<Vec<u8>, CryptoError> {
    let plaintext = name.as_bytes();
    let master = agreement(&ephemeral.private_key, identity_public)?;
    let iv = synthetic_iv(&master, plaintext);
    let mut ciphertext = plaintext.to_vec();
    apply_ctr(&master, &iv, &mut ciphertext);

    let encoded = DeviceName {
        ephemeral_public: ephemeral.public_key.serialize().to_vec(),
        synthetic_iv: iv.to_vec(),
        ciphertext,
    }
    .encode_to_vec();
    if encoded.len() > MAX_ENCODED_DEVICE_NAME_LEN {
        return Err(CryptoError::EncryptionFailed(format!(
            "device name too long: encodes to {} bytes, limit {MAX_ENCODED_DEVICE_NAME_LEN}",
            encoded.len()
        )));
    }
    Ok(encoded)
}

/// Decrypt a serialized `DeviceName` with the account identity key pair.
/// Fails on a malformed encoding, a wrong key, or any tampering with the
/// ciphertext or synthetic IV (the recomputed IV will not match).
pub fn decrypt_device_name(
    encoded: &[u8],
    account_identity: &IdentityKeyPair,
) -> Result<String, CryptoError> {
    let msg = DeviceName::decode(encoded).map_err(|e| {
        CryptoError::DecryptionFailed(format!("device name: malformed encoding: {e}"))
    })?;
    if msg.synthetic_iv.len() != SYNTHETIC_IV_LEN {
        return Err(CryptoError::DecryptionFailed(
            "device name: synthetic IV must be 16 bytes".into(),
        ));
    }
    let ephemeral_public = PublicKey::deserialize(&msg.ephemeral_public).map_err(|e| {
        CryptoError::DecryptionFailed(format!("device name: bad ephemeral key: {e}"))
    })?;

    let master = agreement(account_identity.private_key(), &ephemeral_public)?;
    let mut plaintext = Zeroizing::new(msg.ciphertext);
    apply_ctr(&master, &msg.synthetic_iv, &mut plaintext);

    let ours = synthetic_iv(&master, &plaintext);
    if !bool::from(ours.ct_eq(&msg.synthetic_iv[..])) {
        return Err(CryptoError::DecryptionFailed(
            "device name: synthetic IV did not match".into(),
        ));
    }
    String::from_utf8(plaintext.to_vec())
        .map_err(|_| CryptoError::DecryptionFailed("device name: plaintext is not UTF-8".into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use libsignal_protocol::PrivateKey;

    fn identity() -> IdentityKeyPair {
        use rand::TryRngCore as _;
        IdentityKeyPair::generate(&mut rand::rngs::OsRng.unwrap_err())
    }

    fn unhex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn round_trip() {
        let id = identity();
        for name in ["v1.19.0 on Windows 10", "", "Alex's 手机 📱"] {
            let enc = encrypt_device_name(name, id.identity_key()).unwrap();
            assert_eq!(decrypt_device_name(&enc, &id).unwrap(), name);
        }
    }

    #[test]
    fn fresh_ephemeral_per_encryption() {
        let id = identity();
        let a = encrypt_device_name("Desktop", id.identity_key()).unwrap();
        let b = encrypt_device_name("Desktop", id.identity_key()).unwrap();
        assert_ne!(a, b);
    }

    fn tampered(enc: &[u8], f: impl FnOnce(&mut DeviceName)) -> Vec<u8> {
        let mut msg = DeviceName::decode(enc).unwrap();
        f(&mut msg);
        msg.encode_to_vec()
    }

    #[test]
    fn tampered_ciphertext_fails() {
        let id = identity();
        let enc = encrypt_device_name("v1.19.0 on Windows 10", id.identity_key()).unwrap();
        let bad = tampered(&enc, |m| m.ciphertext[0] ^= 0x01);
        let err = decrypt_device_name(&bad, &id).unwrap_err();
        assert!(
            err.to_string().contains("synthetic IV did not match"),
            "{err}"
        );
    }

    #[test]
    fn tampered_synthetic_iv_fails() {
        let id = identity();
        let enc = encrypt_device_name("v1.19.0 on Windows 10", id.identity_key()).unwrap();
        let bad = tampered(&enc, |m| m.synthetic_iv[15] ^= 0x80);
        let err = decrypt_device_name(&bad, &id).unwrap_err();
        assert!(
            err.to_string().contains("synthetic IV did not match"),
            "{err}"
        );
    }

    #[test]
    fn other_accounts_identity_cannot_decrypt() {
        let ours = identity();
        let theirs = identity();
        let enc = encrypt_device_name("v1.19.0 on Windows 10", ours.identity_key()).unwrap();
        assert!(decrypt_device_name(&enc, &theirs).is_err());
    }

    #[test]
    fn malformed_encodings_fail() {
        let id = identity();
        assert!(decrypt_device_name(&[0xff, 0xff], &id).is_err());
        let enc = encrypt_device_name("x", id.identity_key()).unwrap();
        assert!(decrypt_device_name(&tampered(&enc, |m| m.synthetic_iv.truncate(8)), &id).is_err());
        assert!(decrypt_device_name(&tampered(&enc, |m| m.ephemeral_public.clear()), &id).is_err());
    }

    #[test]
    fn length_bound_matches_server() {
        let id = identity();
        let max = "a".repeat(169);
        let enc = encrypt_device_name(&max, id.identity_key()).unwrap();
        assert!(enc.len() <= MAX_ENCODED_DEVICE_NAME_LEN);
        assert!(encrypt_device_name(&"a".repeat(170), id.identity_key()).is_err());
    }

    /// Known-answer vector from an independent re-implementation of
    /// Signal-Desktop `encryptDeviceName` on node:crypto (X25519, HMAC-SHA256,
    /// aes-256-ctr with a zero counter) with fixed keys. Signal-Desktop's own
    /// tests (ts/test-electron/Crypto_test.preload.ts) only round-trip.
    #[test]
    fn known_answer_vector() {
        let id_priv = PrivateKey::deserialize(&unhex(
            "a81f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f",
        ))
        .unwrap();
        let eph_priv = PrivateKey::deserialize(&unhex(
            "482e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e",
        ))
        .unwrap();
        let id_pub = id_priv.public_key().unwrap();
        let eph = KeyPair::new(eph_priv.public_key().unwrap(), eph_priv);
        let mut want_id_pub = vec![0x05];
        want_id_pub.extend(unhex(
            "4e474f99a20034bfa43290757c2f08fdbd40a8f1d6f4a5512b9c4ab46b5cc640",
        ));
        assert_eq!(id_pub.serialize().to_vec(), want_id_pub);

        let enc = encrypt_with_ephemeral("v1.19.0 on Windows 10", &id_pub, &eph).unwrap();
        let msg = DeviceName::decode(&enc[..]).unwrap();
        let mut want_eph = vec![0x05];
        want_eph.extend(unhex(
            "951806acbc98909b64cbcd52bd5f9734650c636f69154ba807d6dab436a58d6d",
        ));
        assert_eq!(msg.ephemeral_public, want_eph);
        assert_eq!(msg.synthetic_iv, unhex("e37f1af6a98c2be5a2f50a34625d7ead"));
        assert_eq!(
            msg.ciphertext,
            unhex("993a58cc39ad3a4d2dcf80f1d16d1b7df6e6b36916")
        );

        let pair = IdentityKeyPair::new(IdentityKey::new(id_pub), id_priv);
        assert_eq!(
            decrypt_device_name(&enc, &pair).unwrap(),
            "v1.19.0 on Windows 10"
        );
    }
}

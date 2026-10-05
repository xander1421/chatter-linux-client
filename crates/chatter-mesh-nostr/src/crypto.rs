//! bitchat's DM encryption. **This is not NIP-44 v2.**
//!
//! bitchat calls this "NIP-44 v2", tags its payloads `v2:`, and diverges from the
//! published specification on every axis:
//!
//! | | Real NIP-44 v2 | bitchat (implemented here) |
//! |---|---|---|
//! | Cipher | ChaCha20 (unauthenticated) + HMAC-SHA256 | XChaCha20-**Poly1305** AEAD |
//! | Nonce | 32 bytes | 24 bytes |
//! | Payload | `base64(0x02 ‖ nonce ‖ ct ‖ mac32)` | `"v2:" + base64url(nonce24 ‖ ct ‖ tag16)` |
//! | HKDF | salt `"nip44-v2"`, no info | salt **empty**, info `"nip44-v2"` |
//! | Padding | power-of-two buckets, min 32 | none |
//! | AAD | n/a | empty |
//!
//! Implementing the specification instead would interoperate with every Nostr
//! client *except* bitchat, which is the one client we are targeting. The
//! divergence is deliberate. Do not "fix" it. See the P4 design doc §2 and §4.3.
//!
//! Ported from `bitchat/Nostr/NostrProtocol.swift` (`encrypt`, `decrypt`,
//! `deriveSharedSecret`, `deriveNIP44V2Key`), `XChaCha20Poly1305Compat.swift`, and
//! `Base64URLCoding.swift`.
//!
//! # Three behaviours that look like bugs and are not
//!
//! 1. **The shared secret is 33 bytes, not 32.** bitchat calls P256K's
//!    `sharedSecretFromKeyAgreement(with:format: .compressed)`, whose ECDH hash
//!    callback does no hashing at all: it writes `0x02 | (y & 1)` followed by the
//!    32-byte x-coordinate — the compressed SEC1 encoding of the ECDH point. That
//!    whole 33-byte value is the HKDF input. The parity byte is load-bearing; see
//!    (2). (`swift-secp256k1` 0.21.1, `Sources/ZKP/ECDH.swift`.)
//!
//! 2. **Decrypt retries both Y-parities.** Nostr publishes x-only pubkeys, so a
//!    peer's Y parity is unknown. Rebuilding the point with the wrong parity gives
//!    the *negated* ECDH point: same x, opposite y — identical x-coordinate but a
//!    different compressed prefix, hence a different conversation key. Whenever the
//!    two parties' pubkeys have differing parity, the first attempt fails and the
//!    `0x03` retry succeeds. This is why (1) matters: with an x-only shared secret
//!    the retry would be dead code.
//!
//! 3. **A payload of exactly 40 bytes is rejected.** bitchat's decrypt guards on
//!    `count > 24 + 16`, strictly greater. Sealing an empty plaintext produces
//!    exactly 24 + 0 + 16 = 40 bytes, so bitchat cannot decrypt its own encryption
//!    of `""`. Reproduced faithfully — see `encrypting_an_empty_plaintext_is_a_dead_letter`.
//!
//! # Error discipline
//!
//! Everything that fails *after* a key is involved collapses to
//! [`NostrError::Decrypt`]: tag mismatch, wrong key, and a failed parity retry are
//! indistinguishable to a caller, because distinguishing them is what a padding- or
//! authentication-oracle attack feeds on. Framing faults — a missing `v2:` prefix,
//! non-base64url input, a payload too short to hold a nonce and a tag — are
//! [`NostrError::InvalidCiphertext`], which is safe to report because detecting them
//! needs no key material. A sender pubkey that is not a point on the curve is
//! [`NostrError::InvalidPublicKey`] for the same reason: its validity is independent
//! of our secret.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chacha20poly1305::aead::Aead as _;
use chacha20poly1305::{Key, KeyInit as _, XChaCha20Poly1305, XNonce};
use hkdf::Hkdf;
use rand_core::CryptoRngCore;
use secp256k1::ecdh::shared_secret_point;
use secp256k1::{PublicKey, SecretKey};
use sha2::Sha256;

use crate::error::NostrError;
use crate::{CIPHERTEXT_PREFIX, NIP44_HKDF_INFO};

/// XChaCha20-Poly1305 nonce length.
pub const NONCE_LEN: usize = 24;
/// Poly1305 authentication tag length.
pub const TAG_LEN: usize = 16;
/// Compressed SEC1 point: `0x02 | (y & 1)` followed by the 32-byte x-coordinate.
/// This is what bitchat feeds to HKDF — not the bare x-coordinate. See the module
/// docs.
pub const SHARED_SECRET_LEN: usize = 33;
/// HKDF-SHA256 output length.
pub const CONVERSATION_KEY_LEN: usize = 32;

/// ECDH shared secret between our secret key and a peer's public key.
///
/// Returns the **33-byte compressed SEC1 encoding** of the ECDH point, matching
/// bitchat's `deriveSharedSecret`. The P4 design doc §4.3 describes this as "x-only,
/// 32 bytes"; that is wrong, and the Y-parity retry in [`decrypt`] only makes sense
/// because it is wrong — see the module docs.
///
/// `public_key_hex` may be x-only (32 bytes, the Nostr wire form), compressed
/// (33 bytes) or uncompressed (65 bytes). For the x-only form bitchat assumes even
/// Y (`0x02`) and falls back to `0x03` only if that point fails to parse.
pub fn shared_secret(
    secret_key: &[u8; 32],
    public_key_hex: &str,
) -> Result<[u8; SHARED_SECRET_LEN], NostrError> {
    let secret = parse_secret(secret_key)?;
    let peer = parse_peer_pubkey(public_key_hex)?;
    Ok(ecdh_compressed(&secret, &peer))
}

/// `HKDF-SHA256(ikm = shared, salt = <empty>, info = "nip44-v2", len = 32)`.
///
/// Salt and info are the reverse of the published NIP-44 v2 spec, which uses
/// `"nip44-v2"` as the *salt* and no info. Getting them the right way round is the
/// entire compatibility question.
///
/// An empty salt is identical to a salt of 32 zero bytes here: HKDF-Extract keys an
/// HMAC-SHA256 with the salt, and HMAC zero-pads any key shorter than its 64-byte
/// block.
pub fn derive_conversation_key(shared: &[u8; SHARED_SECRET_LEN]) -> [u8; CONVERSATION_KEY_LEN] {
    let mut key = [0u8; CONVERSATION_KEY_LEN];
    // `expand` rejects only outputs longer than 255 * 32 bytes; this one is 32, so
    // the error arm is unreachable. It is matched rather than unwrapped because the
    // crate denies panicking constructs outside tests.
    if Hkdf::<Sha256>::new(Some(&[]), shared)
        .expand(NIP44_HKDF_INFO, &mut key)
        .is_err()
    {
        return [0u8; CONVERSATION_KEY_LEN];
    }
    key
}

/// Encrypts `plaintext` to `recipient_pubkey_hex`, producing
/// `"v2:" + base64url(nonce24 ‖ ciphertext ‖ tag16)`.
///
/// The nonce is 24 fresh random bytes. Reusing one with the same conversation key
/// would leak the XOR of two plaintexts and forge the tag, which is why the nonce
/// source is a parameter rather than a global — callers must pass a CSPRNG, and
/// [`encrypt_with_nonce`] exists only so tests can pin one.
pub fn encrypt(
    plaintext: &str,
    recipient_pubkey_hex: &str,
    sender_secret: &[u8; 32],
    rng: &mut impl CryptoRngCore,
) -> Result<String, NostrError> {
    let shared = shared_secret(sender_secret, recipient_pubkey_hex)?;
    let conversation_key = derive_conversation_key(&shared);

    let mut nonce = [0u8; NONCE_LEN];
    rng.fill_bytes(&mut nonce);

    encrypt_with_nonce(plaintext.as_bytes(), &conversation_key, &nonce)
}

/// Decrypts a `"v2:"` payload from `sender_pubkey_hex`.
///
/// When the sender key is x-only (32 bytes) this tries the `0x02` point first and,
/// on any failure, the `0x03` point — bitchat's compatibility behaviour, required
/// whenever the two parties' pubkeys have differing Y parity. Both attempts failing
/// yields [`NostrError::Decrypt`] with no indication of which branch got closer.
pub fn decrypt(
    ciphertext: &str,
    sender_pubkey_hex: &str,
    recipient_secret: &[u8; 32],
) -> Result<String, NostrError> {
    let (nonce, sealed) = decode_payload(ciphertext)?;
    let secret = parse_secret(recipient_secret)?;
    let sender = decode_hex(sender_pubkey_hex)?;

    let plaintext = match sender.len() {
        32 => {
            let mut compressed = [0u8; SHARED_SECRET_LEN];
            compressed[1..].copy_from_slice(&sender);

            let mut opened = None;
            let mut parsed_any = false;
            // Even Y first, then odd Y. Order matters only for speed: same-parity
            // peers — the common case — succeed on the first attempt.
            for prefix in [0x02u8, 0x03u8] {
                compressed[0] = prefix;
                let Ok(peer) = PublicKey::from_byte_array_compressed(&compressed) else {
                    continue;
                };
                parsed_any = true;
                let key = derive_conversation_key(&ecdh_compressed(&secret, &peer));
                if let Ok(pt) = open(&key, &nonce, &sealed) {
                    opened = Some(pt);
                    break;
                }
            }

            if !parsed_any {
                // x is not a curve point under either parity. Key-independent, so
                // reporting it is not an oracle.
                return Err(NostrError::InvalidPublicKey);
            }
            opened.ok_or(NostrError::Decrypt)?
        }
        _ => {
            let peer = parse_peer_pubkey(sender_pubkey_hex)?;
            let key = derive_conversation_key(&ecdh_compressed(&secret, &peer));
            open(&key, &nonce, &sealed)?
        }
    };

    // bitchat maps non-UTF-8 plaintext to the empty string. We reject instead: the
    // payload is already authenticated at this point, so this is not an oracle, and
    // silently returning "" would make a corrupt message indistinguishable from an
    // empty one.
    String::from_utf8(plaintext).map_err(|_| NostrError::Decrypt)
}

/// The deterministic core of [`encrypt`], with the nonce supplied by the caller.
///
/// Exposed so tests can pin a nonce and compare payloads byte for byte. Production
/// callers want [`encrypt`] — a repeated nonce under the same conversation key is a
/// total break of confidentiality *and* authenticity.
pub fn encrypt_with_nonce(
    plaintext: &[u8],
    conversation_key: &[u8; CONVERSATION_KEY_LEN],
    nonce24: &[u8; NONCE_LEN],
) -> Result<String, NostrError> {
    let key = Key::from(*conversation_key);
    let nonce = XNonce::from(*nonce24);

    // `Aead::encrypt` with a bare slice uses an empty AAD, which is what bitchat
    // passes, and returns `ciphertext ‖ tag` — already the tail of the wire layout.
    let sealed = XChaCha20Poly1305::new(&key)
        .encrypt(&nonce, plaintext)
        .map_err(|_| NostrError::Encrypt)?;

    let mut payload = Vec::with_capacity(NONCE_LEN + sealed.len());
    payload.extend_from_slice(nonce24);
    payload.extend_from_slice(&sealed);

    // base64url, unpadded: `Base64URLCoding.encode` strips every '='.
    Ok(format!(
        "{CIPHERTEXT_PREFIX}{}",
        URL_SAFE_NO_PAD.encode(&payload)
    ))
}

// ---------------------------------------------------------------------------
// internals
// ---------------------------------------------------------------------------

fn parse_secret(secret_key: &[u8; 32]) -> Result<SecretKey, NostrError> {
    SecretKey::from_byte_array(secret_key).map_err(|_| NostrError::InvalidPrivateKey)
}

/// Mirrors `deriveSharedSecret`'s public-key handling: x-only keys get a `0x02`
/// prefix, falling back to `0x03` if the even-Y point does not parse.
fn parse_peer_pubkey(public_key_hex: &str) -> Result<PublicKey, NostrError> {
    let bytes = decode_hex(public_key_hex)?;
    match bytes.len() {
        32 => {
            let mut compressed = [0u8; SHARED_SECRET_LEN];
            compressed[0] = 0x02;
            compressed[1..].copy_from_slice(&bytes);
            if let Ok(key) = PublicKey::from_byte_array_compressed(&compressed) {
                return Ok(key);
            }
            compressed[0] = 0x03;
            PublicKey::from_byte_array_compressed(&compressed)
                .map_err(|_| NostrError::InvalidPublicKey)
        }
        33 | 65 => PublicKey::from_slice(&bytes).map_err(|_| NostrError::InvalidPublicKey),
        _ => Err(NostrError::InvalidPublicKey),
    }
}

/// ECDH, serialized the way P256K's `.compressed` key agreement serializes it:
/// `0x02 | (y & 1)` followed by x. Deliberately *not* `secp256k1::ecdh::SharedSecret`,
/// which would SHA-256 the point and produce a key bitchat cannot derive.
fn ecdh_compressed(secret: &SecretKey, peer: &PublicKey) -> [u8; SHARED_SECRET_LEN] {
    let xy = shared_secret_point(peer, secret);
    let mut out = [0u8; SHARED_SECRET_LEN];
    out[0] = 0x02 | (xy[63] & 0x01);
    out[1..].copy_from_slice(&xy[..32]);
    out
}

fn open(
    conversation_key: &[u8; CONVERSATION_KEY_LEN],
    nonce24: &[u8; NONCE_LEN],
    sealed: &[u8],
) -> Result<Vec<u8>, NostrError> {
    let key = Key::from(*conversation_key);
    let nonce = XNonce::from(*nonce24);
    XChaCha20Poly1305::new(&key)
        .decrypt(&nonce, sealed)
        .map_err(|_| NostrError::Decrypt)
}

/// Splits `"v2:" + base64url(nonce24 ‖ ct ‖ tag16)` into its nonce and its
/// `ciphertext ‖ tag` tail.
fn decode_payload(ciphertext: &str) -> Result<([u8; NONCE_LEN], Vec<u8>), NostrError> {
    let encoded = ciphertext
        .strip_prefix(CIPHERTEXT_PREFIX)
        .ok_or(NostrError::InvalidCiphertext)?;
    let bytes = decode_base64url(encoded)?;

    // Strictly greater, as bitchat has it. 40 bytes exactly — a sealed empty
    // plaintext — is rejected. See the module docs.
    if bytes.len() <= NONCE_LEN + TAG_LEN {
        return Err(NostrError::InvalidCiphertext);
    }

    let mut nonce = [0u8; NONCE_LEN];
    nonce.copy_from_slice(&bytes[..NONCE_LEN]);
    Ok((nonce, bytes[NONCE_LEN..].to_vec()))
}

/// `Base64URLCoding.decode` re-pads before decoding, so it accepts padded and
/// unpadded input alike. We strip padding and decode unpadded, which accepts both.
///
/// One deliberate narrowing: Swift maps `-`/`_` to `+`/`/` and then runs a *standard*
/// base64 decoder, so it would also accept the standard alphabet. bitchat's encoder
/// never emits it, so we stay strict to base64url.
fn decode_base64url(encoded: &str) -> Result<Vec<u8>, NostrError> {
    URL_SAFE_NO_PAD
        .decode(encoded.trim_end_matches('='))
        .map_err(|_| NostrError::InvalidCiphertext)
}

/// Mirrors `Data(hexString:)`: trims whitespace, tolerates a `0x`/`0X` prefix,
/// rejects odd lengths and non-hex characters. Case-insensitive.
fn decode_hex(value: &str) -> Result<Vec<u8>, NostrError> {
    let trimmed = value.trim();
    let body = trimmed
        .strip_prefix("0x")
        .or_else(|| trimmed.strip_prefix("0X"))
        .unwrap_or(trimmed);
    hex::decode(body).map_err(|_| NostrError::InvalidPublicKey)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chacha20poly1305::aead::Payload;
    use sha2::Digest as _;

    // Keypairs generated for these tests, chosen so that both Y parities appear.
    // Cross-checked against an independent pure-Python secp256k1 implementation.
    const EVEN_A_SEC: [u8; 32] =
        hexlit("42f142b892e3bab20ad49b3602d0be46d1dd512b48e9090a0f19f353ad2139ff");
    const EVEN_A_XONLY: &str = "31a6199901ca9461883c53cfc4a1a2212ce428f124c097364dcef87308645d5d";
    const EVEN_B_SEC: [u8; 32] =
        hexlit("691e4c43b325c95617a29b95580932fa861ed9df734188418645fa8232afcafc");
    const EVEN_B_XONLY: &str = "05cee4252077b34f3e8cbb52f95ec0830e9b84e8900a363ef29408405a79e297";
    const ODD_A_SEC: [u8; 32] =
        hexlit("4b3303aeb195c023f5882b4bf9b70a58b5fdb02bd1c1ff297004c169882dae1e");
    const ODD_A_XONLY: &str = "ed7ba4019f63abc292aedc4f2a3860679d03dcc097f82fae05bb497622f8387b";
    const ODD_B_SEC: [u8; 32] =
        hexlit("caf9cccc1d0d94f033378b84808d830f3372d3a9102ac185ca0fead77454bcc1");
    const ODD_B_XONLY: &str = "86b3903656f3438059c6359ba6a795d341b571889d5be7779777620a6677d0fb";

    /// `const`-evaluable hex decode, so key material can live in `const` items.
    const fn hexlit(s: &str) -> [u8; 32] {
        let bytes = s.as_bytes();
        assert!(bytes.len() == 64, "expected 64 hex characters");
        let mut out = [0u8; 32];
        let mut i = 0;
        while i < 32 {
            out[i] = nibble(bytes[i * 2]) << 4 | nibble(bytes[i * 2 + 1]);
            i += 1;
        }
        out
    }

    const fn nibble(c: u8) -> u8 {
        match c {
            b'0'..=b'9' => c - b'0',
            b'a'..=b'f' => c - b'a' + 10,
            b'A'..=b'F' => c - b'A' + 10,
            _ => panic!("not a hex digit"),
        }
    }

    fn hexv(s: &str) -> Vec<u8> {
        hex::decode(s).unwrap()
    }

    /// A CSPRNG-shaped stub that emits a fixed byte sequence. Test-only: it is not
    /// random, which is exactly the point — it pins the nonce so payloads are
    /// comparable byte for byte.
    struct FixedRng {
        bytes: Vec<u8>,
        cursor: usize,
    }

    impl FixedRng {
        fn new(bytes: Vec<u8>) -> Self {
            Self { bytes, cursor: 0 }
        }
    }

    impl rand_core::RngCore for FixedRng {
        fn next_u32(&mut self) -> u32 {
            let mut b = [0u8; 4];
            self.fill_bytes(&mut b);
            u32::from_le_bytes(b)
        }

        fn next_u64(&mut self) -> u64 {
            let mut b = [0u8; 8];
            self.fill_bytes(&mut b);
            u64::from_le_bytes(b)
        }

        fn fill_bytes(&mut self, dest: &mut [u8]) {
            for slot in dest.iter_mut() {
                *slot = self.bytes[self.cursor % self.bytes.len()];
                self.cursor += 1;
            }
        }

        fn try_fill_bytes(&mut self, dest: &mut [u8]) -> Result<(), rand_core::Error> {
            self.fill_bytes(dest);
            Ok(())
        }
    }

    impl rand_core::CryptoRng for FixedRng {}

    fn nonce_rng(nonce: [u8; NONCE_LEN]) -> FixedRng {
        FixedRng::new(nonce.to_vec())
    }

    fn sequential_nonce(start: u8) -> [u8; NONCE_LEN] {
        let mut nonce = [0u8; NONCE_LEN];
        for (i, slot) in nonce.iter_mut().enumerate() {
            *slot = start.wrapping_add(i as u8);
        }
        nonce
    }

    // -----------------------------------------------------------------
    // The primitive: XChaCha20-Poly1305 against a published vector
    // -----------------------------------------------------------------

    /// draft-irtf-cfrg-xchacha-03 §A.3.1, the AEAD_XCHACHA20_POLY1305 vector.
    ///
    /// bitchat hand-rolls HChaCha20 in `XChaCha20Poly1305Compat.swift` and reduces
    /// the 24-byte nonce to `0x00000000 ‖ nonce[16..24]`, which is exactly the
    /// standard construction the `chacha20poly1305` crate implements. Pinning the
    /// published vector proves that equivalence: a mismatch here is unambiguous.
    #[test]
    fn xchacha20poly1305_matches_the_published_draft_vector() {
        let key = hexv("808182838485868788898a8b8c8d8e8f909192939495969798999a9b9c9d9e9f");
        let nonce = hexv("404142434445464748494a4b4c4d4e4f5051525354555657");
        let aad = hexv("50515253c0c1c2c3c4c5c6c7");
        let plaintext = b"Ladies and Gentlemen of the class of '99: If I could offer you \
only one tip for the future, sunscreen would be it.";

        let cipher = XChaCha20Poly1305::new(Key::from_slice(&key));
        let sealed = cipher
            .encrypt(
                XNonce::from_slice(&nonce),
                Payload {
                    msg: &plaintext[..],
                    aad: &aad[..],
                },
            )
            .unwrap();

        let (ciphertext, tag) = sealed.split_at(sealed.len() - TAG_LEN);
        assert_eq!(
            hex::encode(ciphertext),
            "bd6d179d3e83d43b9576579493c0e939572a1700252bfaccbed2902c21396cbb\
731c7f1b0b4aa6440bf3a82f4eda7e39ae64c6708c54c216cb96b72e1213b452\
2f8c9ba40db5d945b11b69b982c1bb9e3f3fac2bc369488f76b2383565d3fff9\
21f9664c97637da9768812f615c68b13b52e"
        );
        assert_eq!(hex::encode(tag), "c0875924c1c7987947deafd8780acf49");
    }

    // -----------------------------------------------------------------
    // ECDH: the 33-byte compressed point, and why parity matters
    // -----------------------------------------------------------------

    #[test]
    fn shared_secret_is_the_compressed_point_not_the_bare_x_coordinate() {
        let shared = shared_secret(&EVEN_A_SEC, EVEN_B_XONLY).unwrap();
        assert_eq!(shared.len(), 33);
        assert_eq!(
            hex::encode(shared),
            "02bb7fc62bd2b7566a23ab6b8e0c19d375da300816f1d61ab5727d3bcf4e1723ce"
        );
        // Symmetric when both pubkeys have even Y: no retry needed.
        assert_eq!(shared, shared_secret(&EVEN_B_SEC, EVEN_A_XONLY).unwrap());
    }

    #[test]
    fn shared_secret_is_symmetric_when_both_keys_have_odd_y() {
        let a = shared_secret(&ODD_A_SEC, ODD_B_XONLY).unwrap();
        let b = shared_secret(&ODD_B_SEC, ODD_A_XONLY).unwrap();
        assert_eq!(a, b);
        assert_eq!(
            hex::encode(a),
            "038e07e2a23856cb28a53064987777c327622a6fd1769f226bd3f0da578921df16"
        );
    }

    /// The reason the parity retry exists. Mixed-parity peers derive points that
    /// share an x-coordinate and differ only in the compressed prefix byte — which
    /// is part of the HKDF input, so the conversation keys differ.
    #[test]
    fn mixed_parity_peers_disagree_on_the_first_attempt() {
        let from_even = shared_secret(&EVEN_A_SEC, ODD_A_XONLY).unwrap();
        let from_odd = shared_secret(&ODD_A_SEC, EVEN_A_XONLY).unwrap();

        assert_ne!(from_even, from_odd);
        assert_eq!(from_even[1..], from_odd[1..], "same x-coordinate");
        assert_eq!(from_even[0], 0x03);
        assert_eq!(from_odd[0], 0x02);
        assert_eq!(
            hex::encode(from_even),
            "0370997fe6f9e03754817f1dbc753472041c4c317cf0fcfae77db16e33de8bce29"
        );
    }

    #[test]
    fn shared_secret_accepts_compressed_and_uncompressed_peer_keys() {
        let x_only = shared_secret(&EVEN_A_SEC, EVEN_B_XONLY).unwrap();

        let compressed = format!("02{EVEN_B_XONLY}");
        assert_eq!(shared_secret(&EVEN_A_SEC, &compressed).unwrap(), x_only);

        let peer = parse_peer_pubkey(EVEN_B_XONLY).unwrap();
        let uncompressed = hex::encode(peer.serialize_uncompressed());
        assert_eq!(shared_secret(&EVEN_A_SEC, &uncompressed).unwrap(), x_only);
    }

    #[test]
    fn shared_secret_tolerates_hex_prefix_and_whitespace() {
        let plain = shared_secret(&EVEN_A_SEC, EVEN_B_XONLY).unwrap();
        assert_eq!(
            shared_secret(&EVEN_A_SEC, &format!("  0x{EVEN_B_XONLY}  ")).unwrap(),
            plain
        );
        assert_eq!(
            shared_secret(&EVEN_A_SEC, &EVEN_B_XONLY.to_uppercase()).unwrap(),
            plain
        );
    }

    #[test]
    fn shared_secret_rejects_malformed_public_keys() {
        for bad in [
            "",
            "zz",
            "abc",                              // odd length
            "31a6199901ca9461883c53cfc4a1a221", // 16 bytes
            &"ff".repeat(32),                   // 32 bytes, not on the curve
        ] {
            assert_eq!(
                shared_secret(&EVEN_A_SEC, bad),
                Err(NostrError::InvalidPublicKey),
                "expected rejection for {bad:?}"
            );
        }
    }

    #[test]
    fn shared_secret_rejects_an_out_of_range_secret() {
        assert_eq!(
            shared_secret(&[0u8; 32], EVEN_B_XONLY),
            Err(NostrError::InvalidPrivateKey)
        );
        assert_eq!(
            shared_secret(&[0xffu8; 32], EVEN_B_XONLY),
            Err(NostrError::InvalidPrivateKey)
        );
    }

    // -----------------------------------------------------------------
    // HKDF
    // -----------------------------------------------------------------

    #[test]
    fn conversation_key_uses_an_empty_salt_and_nip44_v2_as_info() {
        let mut ikm = [0x42u8; SHARED_SECRET_LEN];
        ikm[0] = 0x02;
        assert_eq!(
            hex::encode(derive_conversation_key(&ikm)),
            "f825edc4f15b82023bde35e4f43c6feacd2373b5dee83990176c9e61590a01b3"
        );
        assert_eq!(
            hex::encode(derive_conversation_key(&[0x42u8; SHARED_SECRET_LEN])),
            "0c0bebbaf02171e4a4486f0242e2d915f96e63ab36f9dba938610485cae95ec8"
        );

        let shared = shared_secret(&EVEN_A_SEC, EVEN_B_XONLY).unwrap();
        assert_eq!(
            hex::encode(derive_conversation_key(&shared)),
            "b9b20ecd916e5f4ab4dc5f8a8f738e34a456ca17110eb322c96d8dad9f3cb6b7"
        );
    }

    /// The one bit flip that would silently break interop with bitchat: swapping
    /// salt and info yields the published-spec ordering and a different key.
    #[test]
    fn swapping_salt_and_info_would_produce_a_different_key() {
        let shared = [0x42u8; SHARED_SECRET_LEN];
        let ours = derive_conversation_key(&shared);

        let mut spec_ordering = [0u8; CONVERSATION_KEY_LEN];
        Hkdf::<Sha256>::new(Some(NIP44_HKDF_INFO), &shared)
            .expand(&[], &mut spec_ordering)
            .unwrap();

        assert_ne!(ours, spec_ordering);
    }

    // -----------------------------------------------------------------
    // encrypt_with_nonce: determinism and fixed payloads
    // -----------------------------------------------------------------

    #[test]
    fn encrypt_with_nonce_matches_reference_payloads() {
        let key = [0x42u8; CONVERSATION_KEY_LEN];
        let nonce = [0x24u8; NONCE_LEN];

        assert_eq!(
            encrypt_with_nonce(b"Hello, XChaCha20-Poly1305!", &key, &nonce).unwrap(),
            "v2:JCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQk7Tu7CQEIbP7hR155Zuk_-h-h4dYFLketFEBbBa_Jdn5gAYo7fFrdc9SB"
        );
        assert_eq!(
            encrypt_with_nonce(b"", &key, &nonce).unwrap(),
            "v2:JCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkNg2aKC9CgLKzUexXnZGfHw"
        );
        assert_eq!(
            encrypt_with_nonce("héllo ☃ 世界 🔐".as_bytes(), &key, &nonce).unwrap(),
            "v2:JCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkzZ1-CQJLbEQ6rB_eth7qX77RfiXoj-TTPYsJBLKj-bpw-dldctw"
        );
    }

    #[test]
    fn encrypt_with_nonce_is_deterministic_and_encrypt_is_not() {
        let key = [0x42u8; CONVERSATION_KEY_LEN];
        let nonce = [0x24u8; NONCE_LEN];

        let first = encrypt_with_nonce(b"same input, same output", &key, &nonce).unwrap();
        let second = encrypt_with_nonce(b"same input, same output", &key, &nonce).unwrap();
        assert_eq!(first, second);

        // A different nonce must change the ciphertext, not just the nonce prefix.
        let other =
            encrypt_with_nonce(b"same input, same output", &key, &[0x25u8; NONCE_LEN]).unwrap();
        assert_ne!(first, other);
        assert_ne!(first[3..40], other[3..40]);
    }

    #[test]
    fn payloads_are_prefixed_unpadded_base64url() {
        let key = [0x42u8; CONVERSATION_KEY_LEN];
        // 1..=40 bytes covers every base64 remainder class, so any padding leak shows.
        for len in 1..=40usize {
            let payload = encrypt_with_nonce(&vec![b'x'; len], &key, &[0x24u8; NONCE_LEN]).unwrap();
            assert!(payload.starts_with("v2:"));
            let body = &payload[3..];
            assert!(!body.contains('='), "padding leaked at len {len}");
            assert!(!body.contains('+'), "standard alphabet leaked at len {len}");
            assert!(!body.contains('/'), "standard alphabet leaked at len {len}");
        }

        // Matches bitchat's own Base64URLCodingTests fixture.
        assert_eq!(URL_SAFE_NO_PAD.encode(hexv("ffeeddcc")), "_-7dzA");
    }

    #[test]
    fn decoder_accepts_padded_and_unpadded_base64url() {
        let key = [0x42u8; CONVERSATION_KEY_LEN];
        // 18 bytes of plaintext -> 58 raw bytes, which is not a multiple of 3, so
        // the padded encoding really does carry '='.
        let unpadded =
            encrypt_with_nonce(b"padding tolerance!", &key, &[0x24u8; NONCE_LEN]).unwrap();

        let raw = decode_base64url(&unpadded[3..]).unwrap();
        let padded = format!(
            "v2:{}",
            base64::engine::general_purpose::URL_SAFE.encode(&raw)
        );
        assert!(padded.contains('='), "fixture should actually be padded");

        assert_eq!(
            decode_payload(&padded).unwrap(),
            decode_payload(&unpadded).unwrap()
        );
    }

    // -----------------------------------------------------------------
    // End-to-end
    // -----------------------------------------------------------------

    #[test]
    fn round_trips_in_both_directions_between_two_even_y_keys() {
        let message = "the mesh is the message";

        let a_to_b = encrypt(
            message,
            EVEN_B_XONLY,
            &EVEN_A_SEC,
            &mut nonce_rng(sequential_nonce(1)),
        )
        .unwrap();
        assert_eq!(
            decrypt(&a_to_b, EVEN_A_XONLY, &EVEN_B_SEC).unwrap(),
            message
        );

        let b_to_a = encrypt(
            message,
            EVEN_A_XONLY,
            &EVEN_B_SEC,
            &mut nonce_rng(sequential_nonce(77)),
        )
        .unwrap();
        assert_eq!(
            decrypt(&b_to_a, EVEN_B_XONLY, &EVEN_A_SEC).unwrap(),
            message
        );
    }

    #[test]
    fn encrypt_matches_the_reference_end_to_end_payload() {
        assert_eq!(
            encrypt(
                "the mesh is the message",
                EVEN_B_XONLY,
                &EVEN_A_SEC,
                &mut nonce_rng(sequential_nonce(1)),
            )
            .unwrap(),
            "v2:AQIDBAUGBwgJCgsMDQ4PEBESExQVFhcY5bdCSEALF9cYysWxLeYtauRm50eA2MBb_kHqCYnBMe-oYP2E8CAn"
        );
    }

    /// Even-Y sender to odd-Y recipient. The recipient's first attempt (`0x02`)
    /// derives the negated point and must fail before the `0x03` retry succeeds.
    #[test]
    fn odd_y_retry_branch_decrypts_a_reference_payload() {
        let payload =
            "v2:ZWZnaGlqa2xtbm9wcXJzdHV2d3h5ent89B1DLwHwLFCtZSelWt5rPoKeKP7O_3VcRH0HA5B_pJnfZNs";
        assert_eq!(
            decrypt(payload, EVEN_A_XONLY, &ODD_A_SEC).unwrap(),
            "parity retry please"
        );

        // And the same payload our own encrypt produces.
        assert_eq!(
            encrypt(
                "parity retry please",
                ODD_A_XONLY,
                &EVEN_A_SEC,
                &mut nonce_rng(sequential_nonce(101)),
            )
            .unwrap(),
            payload
        );
    }

    /// Odd-Y sender to even-Y recipient — the mirror image, also requiring `0x03`.
    #[test]
    fn odd_y_sender_to_even_y_recipient_also_needs_the_retry() {
        let payload =
            "v2:ycrLzM3Oz9DR0tPU1dbX2Nna29zd3t_gFxon060a5Wec-gIrmAmQDKeLp9i6WpeXX8oGyYIvtH7t";
        assert_eq!(
            encrypt(
                "from the odd side",
                EVEN_A_XONLY,
                &ODD_A_SEC,
                &mut nonce_rng(sequential_nonce(201)),
            )
            .unwrap(),
            payload
        );
        assert_eq!(
            decrypt(payload, ODD_A_XONLY, &EVEN_A_SEC).unwrap(),
            "from the odd side"
        );

        // The naive single-parity path really is wrong here: what the recipient
        // computes on its first (0x02) attempt is not the sender's key.
        let senders_key =
            derive_conversation_key(&shared_secret(&ODD_A_SEC, EVEN_A_XONLY).unwrap());
        let first_attempt =
            derive_conversation_key(&shared_secret(&EVEN_A_SEC, ODD_A_XONLY).unwrap());
        assert_ne!(senders_key, first_attempt);
    }

    /// Same-parity peers must succeed without ever reaching the retry.
    #[test]
    fn same_parity_peers_never_need_the_retry() {
        for (sender_sec, sender_pub, recipient_sec, recipient_pub) in [
            (EVEN_A_SEC, EVEN_A_XONLY, EVEN_B_SEC, EVEN_B_XONLY),
            (ODD_A_SEC, ODD_A_XONLY, ODD_B_SEC, ODD_B_XONLY),
        ] {
            let payload = encrypt(
                "no retry",
                recipient_pub,
                &sender_sec,
                &mut nonce_rng(sequential_nonce(9)),
            )
            .unwrap();
            assert_eq!(
                decrypt(&payload, sender_pub, &recipient_sec).unwrap(),
                "no retry"
            );

            // The 0x02 point is the one that works, so the first attempt hits.
            assert_eq!(
                shared_secret(&sender_sec, recipient_pub).unwrap(),
                shared_secret(&recipient_sec, sender_pub).unwrap()
            );
        }
    }

    #[test]
    fn decrypt_accepts_a_compressed_sender_key() {
        let payload = encrypt(
            "explicit parity",
            EVEN_B_XONLY,
            &EVEN_A_SEC,
            &mut nonce_rng(sequential_nonce(5)),
        )
        .unwrap();

        // EVEN_A has even Y, so 0x02 is the correct explicit prefix.
        let compressed = format!("02{EVEN_A_XONLY}");
        assert_eq!(
            decrypt(&payload, &compressed, &EVEN_B_SEC).unwrap(),
            "explicit parity"
        );

        // With an explicit prefix there is no retry: the wrong one just fails.
        let wrong = format!("03{EVEN_A_XONLY}");
        assert_eq!(
            decrypt(&payload, &wrong, &EVEN_B_SEC),
            Err(NostrError::Decrypt)
        );
    }

    #[test]
    fn round_trips_non_ascii_plaintext() {
        let message = "héllo ☃ 世界 🔐 — ЖУРНАЛ — مرحبا";
        let payload = encrypt(
            message,
            EVEN_B_XONLY,
            &EVEN_A_SEC,
            &mut nonce_rng(sequential_nonce(31)),
        )
        .unwrap();
        assert_eq!(
            decrypt(&payload, EVEN_A_XONLY, &EVEN_B_SEC).unwrap(),
            message
        );
    }

    #[test]
    fn round_trips_multi_kilobyte_plaintext() {
        let message = "the quick brown fox jumps over the lazy dog. ".repeat(100);
        assert_eq!(message.len(), 4500);

        let payload = encrypt(
            &message,
            EVEN_B_XONLY,
            &EVEN_A_SEC,
            &mut nonce_rng(sequential_nonce(64)),
        )
        .unwrap();
        assert_eq!(
            decrypt(&payload, EVEN_A_XONLY, &EVEN_B_SEC).unwrap(),
            message
        );

        // No padding: the payload is exactly base64url(24 + len + 16) plus "v2:".
        let expected_body = (NONCE_LEN + message.len() + TAG_LEN).div_ceil(3) * 4
            - match (NONCE_LEN + message.len() + TAG_LEN) % 3 {
                0 => 0,
                1 => 2,
                _ => 1,
            };
        assert_eq!(payload.len(), 3 + expected_body);

        // Fixed-nonce vector for the same plaintext, cross-checked against the
        // independent reference implementation.
        let pinned = encrypt_with_nonce(
            message.as_bytes(),
            &[0x42u8; CONVERSATION_KEY_LEN],
            &[0x24u8; NONCE_LEN],
        )
        .unwrap();
        assert_eq!(pinned.len(), 6057);
        assert_eq!(
            hex::encode(Sha256::digest(pinned.as_bytes())),
            "84e60f1bceaaba32f94c1acf08224a6c9c216e90bcef602aa5ef8dcb82909eb8"
        );
    }

    /// bitchat's `count > 24 + 16` guard is strict, so a sealed empty plaintext —
    /// exactly 40 bytes — is unreadable by bitchat *and* by us. Encryption still
    /// succeeds; the payload is simply a dead letter. Faithful, not a bug.
    #[test]
    fn encrypting_an_empty_plaintext_is_a_dead_letter() {
        let payload = encrypt(
            "",
            EVEN_B_XONLY,
            &EVEN_A_SEC,
            &mut nonce_rng(sequential_nonce(3)),
        )
        .unwrap();

        let raw = decode_base64url(&payload[3..]).unwrap();
        assert_eq!(raw.len(), NONCE_LEN + TAG_LEN);
        assert_eq!(
            decrypt(&payload, EVEN_A_XONLY, &EVEN_B_SEC),
            Err(NostrError::InvalidCiphertext)
        );

        // One byte of plaintext is enough to clear the guard.
        let payload = encrypt(
            " ",
            EVEN_B_XONLY,
            &EVEN_A_SEC,
            &mut nonce_rng(sequential_nonce(3)),
        )
        .unwrap();
        assert_eq!(decrypt(&payload, EVEN_A_XONLY, &EVEN_B_SEC).unwrap(), " ");
    }

    // -----------------------------------------------------------------
    // Failure modes
    // -----------------------------------------------------------------

    fn tampered(payload: &str, index: usize) -> String {
        let mut raw = decode_base64url(&payload[3..]).unwrap();
        raw[index] ^= 0xff;
        format!("v2:{}", URL_SAFE_NO_PAD.encode(&raw))
    }

    #[test]
    fn tampering_with_nonce_ciphertext_or_tag_fails_authentication() {
        let payload = encrypt(
            "authenticate me",
            EVEN_B_XONLY,
            &EVEN_A_SEC,
            &mut nonce_rng(sequential_nonce(17)),
        )
        .unwrap();
        let raw_len = decode_base64url(&payload[3..]).unwrap().len();

        // nonce byte, first ciphertext byte, last tag byte
        for index in [0, NONCE_LEN, raw_len - 1] {
            assert_eq!(
                decrypt(&tampered(&payload, index), EVEN_A_XONLY, &EVEN_B_SEC),
                Err(NostrError::Decrypt),
                "tampering at byte {index} was not caught"
            );
        }
    }

    #[test]
    fn truncating_the_tag_fails_authentication() {
        let payload = encrypt(
            "authenticate me",
            EVEN_B_XONLY,
            &EVEN_A_SEC,
            &mut nonce_rng(sequential_nonce(17)),
        )
        .unwrap();
        let mut raw = decode_base64url(&payload[3..]).unwrap();
        raw.pop();
        let truncated = format!("v2:{}", URL_SAFE_NO_PAD.encode(&raw));

        assert_eq!(
            decrypt(&truncated, EVEN_A_XONLY, &EVEN_B_SEC),
            Err(NostrError::Decrypt)
        );
    }

    #[test]
    fn the_wrong_recipient_key_yields_an_undifferentiated_decrypt_error() {
        let payload = encrypt(
            "not for you",
            EVEN_B_XONLY,
            &EVEN_A_SEC,
            &mut nonce_rng(sequential_nonce(23)),
        )
        .unwrap();

        // Wrong recipient secret, and wrong claimed sender: same error either way.
        assert_eq!(
            decrypt(&payload, EVEN_A_XONLY, &ODD_B_SEC),
            Err(NostrError::Decrypt)
        );
        assert_eq!(
            decrypt(&payload, ODD_B_XONLY, &EVEN_B_SEC),
            Err(NostrError::Decrypt)
        );
    }

    #[test]
    fn malformed_framing_is_rejected_as_invalid_ciphertext() {
        let valid = encrypt(
            "well formed",
            EVEN_B_XONLY,
            &EVEN_A_SEC,
            &mut nonce_rng(sequential_nonce(11)),
        )
        .unwrap();
        let body = valid[3..].to_string();

        let cases: Vec<String> = vec![
            String::new(),                     // empty
            body.clone(),                      // no "v2:" prefix
            format!("v1:{body}"),              // wrong version
            format!("V2:{body}"),              // prefix is case sensitive
            format!(" v2:{body}"),             // no leading-whitespace tolerance
            "v2:".to_string(),                 // prefix only
            "v2:!!!not base64!!!".to_string(), // non-base64url characters
            format!("v2:{}", "+".repeat(60)),  // standard alphabet, not base64url
            format!("v2:{}", "A".repeat(52)),  // decodes to 39 bytes: one short
            format!("v2:{}", "A".repeat(54)),  // decodes to 40 bytes: the strict guard
            format!("v2:{}", "A".repeat(53)),  // 53 base64 chars is not decodable at all
            format!("v2:{}", URL_SAFE_NO_PAD.encode([0u8; 8])),
        ];

        for case in cases {
            assert_eq!(
                decrypt(&case, EVEN_A_XONLY, &EVEN_B_SEC),
                Err(NostrError::InvalidCiphertext),
                "expected InvalidCiphertext for {case:?}"
            );
        }
    }

    #[test]
    fn a_malformed_sender_key_is_reported_separately_from_decryption_failure() {
        let payload = encrypt(
            "well formed",
            EVEN_B_XONLY,
            &EVEN_A_SEC,
            &mut nonce_rng(sequential_nonce(13)),
        )
        .unwrap();

        for bad in ["", "nothex", &"ff".repeat(32), &"ab".repeat(20)] {
            assert_eq!(
                decrypt(&payload, bad, &EVEN_B_SEC),
                Err(NostrError::InvalidPublicKey),
                "expected InvalidPublicKey for {bad:?}"
            );
        }

        assert_eq!(
            decrypt(&payload, EVEN_A_XONLY, &[0u8; 32]),
            Err(NostrError::InvalidPrivateKey)
        );
    }

    /// Framing is checked before any key is touched, so a malformed payload cannot
    /// be used to probe key validity and vice versa.
    #[test]
    fn framing_is_validated_before_key_material() {
        assert_eq!(
            decrypt("not even v2", "nothex", &[0u8; 32]),
            Err(NostrError::InvalidCiphertext)
        );
    }

    #[test]
    fn decrypt_never_panics_on_arbitrary_input() {
        let inputs = [
            "v2:",
            "v2:A",
            "v2:AA",
            "v2:AAA",
            "v2:====",
            "v2:\u{0}\u{1}",
            "v2:🔐🔐🔐",
            "\u{0}",
            "v2",
            "v",
        ];
        for input in inputs {
            let _ = decrypt(input, EVEN_A_XONLY, &EVEN_B_SEC);
            let _ = decrypt(input, "", &[0u8; 32]);
        }
    }
}

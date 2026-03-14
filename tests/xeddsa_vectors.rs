//! Generate XEdDSA test vectors for cross-language verification.
//!
//! Run: cargo test --test xeddsa_vectors -- --nocapture

use libsignal_protocol::IdentityKeyPair;

#[test]
fn generate_xeddsa_test_vectors() {
    let message = b"test challenge nonce 1234567890abcdef";

    // Generate multiple key pairs and sign the same message
    // This covers both sign_bit=0 and sign_bit=1 cases
    for i in 0..20 {
        let key_pair = IdentityKeyPair::generate(&mut rand::rng());

        // Get Curve25519 public key (strip 0x05 prefix)
        let identity_pub_full = key_pair.identity_key().serialize();
        assert_eq!(identity_pub_full[0], 0x05, "Expected 0x05 prefix");
        let curve25519_pub = &identity_pub_full[1..]; // 32 bytes

        // Sign using XEdDSA (same as registration challenge flow)
        let signature = key_pair
            .private_key()
            .calculate_signature(message, &mut rand::rng())
            .expect("signing failed");

        let sign_bit = (signature[63] & 0x80) >> 7;

        println!(
            "VECTOR {} sign_bit={} pubkey={} message={} signature={}",
            i,
            sign_bit,
            hex::encode(curve25519_pub),
            hex::encode(message),
            hex::encode(signature),
        );
    }
}

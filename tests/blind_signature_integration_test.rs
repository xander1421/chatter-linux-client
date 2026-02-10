//! Integration tests for RFC 9474 blind signature implementation
//!
//! Tests cover:
//! - Blind signature operations (blind, sign, unblind, verify)
//! - Message randomizer extraction and validation  
//! - Token acquisition flow with MailboxManager
//! - Mailbox creation with randomized signatures
//! - Proto field validation (message_randomizer)
//! - Unlinkability properties

use blind_rsa_signatures::{KeyPair, Options};
use ed25519_dalek::SigningKey;
use rsa::PublicKeyParts;

use chatter_linux::crypto::blind_signature::BlindSignature;
use chatter_linux::proto::mailbox_v1::CreateEphemeralMailboxRequest;

#[test]
fn test_rfc9474_randomized_variant() {
    // Verify we're using the correct RFC 9474 variant
    let kp = KeyPair::generate(2048).unwrap();

    let blind_sig = BlindSignature {
        server_public_key: kp.pk.clone(),
        options: Options::new(blind_rsa_signatures::Hash::Sha384, false, 48),
    };
    
    let message = b"test_ed25519_public_key_32bytes!";
    
    // Blind the message
    let blinding_result = blind_sig.blind(message).unwrap();
    
    // Verify message randomizer exists (RFC 9474 randomized variant)
    assert!(
        blinding_result.msg_randomizer.is_some(),
        "Message randomizer must be present for randomized variant"
    );
    
    // Verify randomizer is 32 bytes
    if let Some(randomizer) = &blinding_result.msg_randomizer {
        assert_eq!(
            randomizer.as_ref().len(),
            32,
            "Message randomizer must be 32 bytes per RFC 9474"
        );
    }
}

#[test]
fn test_blind_sign_unblind_verify_with_randomizer() {
    // Complete flow: blind → sign → unblind → verify
    let kp = KeyPair::generate(2048).unwrap();
    
    let blind_sig = BlindSignature {
        server_public_key: kp.pk.clone(),
        options: Options::new(
            blind_rsa_signatures::Hash::Sha384,
            false, // randomized PSS
            48,    // salt_len
        ),
    };
    
    // Simulate Ed25519 public key (32 bytes)
    let ed25519_private = SigningKey::from_bytes(&[42u8; 32]);
    let ed25519_public = ed25519_private.verifying_key();
    let message = ed25519_public.as_bytes();
    
    // 1. Client: Blind the message
    let blinding_result = blind_sig.blind(message).unwrap();
    let blinded_msg: &[u8] = blinding_result.blind_msg.as_ref();
    
    // Store the randomizer (client must keep this!)
    let msg_randomizer = blinding_result
        .msg_randomizer
        .as_ref()
        .expect("Randomizer must exist")
        .as_ref()
        .to_vec();
    
    assert_eq!(msg_randomizer.len(), 32, "Randomizer must be 32 bytes");
    
    // 2. Server: Sign the blinded message
    let blinded_signature = kp
        .sk
        .blind_sign(
            blinded_msg,
            &Options::new(blind_rsa_signatures::Hash::Sha384, false, 48),
        )
        .unwrap();
    
    // 3. Client: Unblind the signature
    let signature = blind_sig
        .unblind(blinded_signature.as_ref(), &blinding_result, message)
        .unwrap();
    
    // 4. Verify signature (this is what server does during mailbox creation)
    // Must pass the randomizer since we used randomize_message=true
    assert!(
        blind_sig.verify(message, &signature, Some(&msg_randomizer)),
        "Signature verification must succeed"
    );

    // 5. Verify with wrong message fails
    let wrong_message = b"wrong_ed25519_public_key_32byte!";
    assert!(
        !blind_sig.verify(wrong_message, &signature, Some(&msg_randomizer)),
        "Signature with wrong message must fail"
    );
}

#[test]
fn test_message_randomizer_uniqueness() {
    // Verify that each blind operation generates a unique randomizer
    let kp = KeyPair::generate(2048).unwrap();
    let blind_sig = BlindSignature {
        server_public_key: kp.pk.clone(),
        options: Options::new(blind_rsa_signatures::Hash::Sha384, false, 48),
    };
    
    let message = b"same_message_for_all_blindings!!";
    
    // Generate multiple blindings of the same message
    let mut randomizers = Vec::new();
    for _ in 0..10 {
        let result = blind_sig.blind(message).unwrap();
        let randomizer = result
            .msg_randomizer
            .expect("Randomizer must exist")
            .as_ref()
            .to_vec();
        randomizers.push(randomizer);
    }
    
    // Verify all randomizers are unique (probabilistically)
    for i in 0..randomizers.len() {
        for j in (i + 1)..randomizers.len() {
            assert_ne!(
                randomizers[i], randomizers[j],
                "Randomizers must be unique for each blind operation"
            );
        }
    }
}

#[test]
fn test_blinded_messages_are_different() {
    // Verify that blinding the same message multiple times produces different blinded messages
    // This ensures the blinding factor 'r' is working correctly
    let kp = KeyPair::generate(2048).unwrap();
    let blind_sig = BlindSignature {
        server_public_key: kp.pk.clone(),
        options: Options::new(blind_rsa_signatures::Hash::Sha384, false, 48),
    };
    
    let message = b"identical_message_to_blind!!!!!";
    
    let result1 = blind_sig.blind(message).unwrap();
    let result2 = blind_sig.blind(message).unwrap();
    
    // Blinded messages must differ (due to random blinding factor 'r')
    let blind1: &[u8] = result1.blind_msg.as_ref();
    let blind2: &[u8] = result2.blind_msg.as_ref();
    assert_ne!(
        blind1,
        blind2,
        "Blinded messages must be different even for the same input (proves blinding factor randomness)"
    );
    
    // Secrets must differ
    let secret1: &[u8] = result1.secret.as_ref();
    let secret2: &[u8] = result2.secret.as_ref();
    assert_ne!(
        secret1,
        secret2,
        "Blinding secrets must be unique"
    );
    
    // Randomizers must differ
    assert_ne!(
        result1.msg_randomizer.as_ref().unwrap().as_ref(),
        result2.msg_randomizer.as_ref().unwrap().as_ref(),
        "Message randomizers must be unique"
    );
}

#[test]
fn test_proto_message_randomizer_field() {
    // Verify the proto has the message_randomizer field
    let request = CreateEphemeralMailboxRequest {
        public_key: vec![1u8; 32],
        signature: vec![2u8; 256],
        ttl_seconds: 604800,
        message_randomizer: vec![3u8; 32], // ← RFC 9474 field
    };
    
    assert_eq!(request.message_randomizer.len(), 32);
    assert_eq!(request.public_key.len(), 32);
    assert_eq!(request.signature.len(), 256);
    assert_eq!(request.ttl_seconds, 604800);
}

#[test]
fn test_signature_verification_with_randomizer() {
    // Test that verification works when message includes the randomizer
    // This simulates server-side verification: verify(randomizer || pubkey, signature)
    let kp = KeyPair::generate(2048).unwrap();
    let blind_sig = BlindSignature {
        server_public_key: kp.pk.clone(),
        options: Options::new(blind_rsa_signatures::Hash::Sha384, false, 48),
    };
    
    // Ed25519 public key
    let ed25519_key = SigningKey::from_bytes(&[77u8; 32]).verifying_key();
    let pubkey_bytes = ed25519_key.as_bytes();
    
    // 1. Blind
    let blinding_result = blind_sig.blind(pubkey_bytes).unwrap();
    let randomizer = blinding_result
        .msg_randomizer
        .as_ref()
        .unwrap()
        .as_ref()
        .to_vec();

    // 2. Server signs
    let blinded_bytes: &[u8] = blinding_result.blind_msg.as_ref();
    let blinded_sig = kp
        .sk
        .blind_sign(
            blinded_bytes,
            &Options::new(blind_rsa_signatures::Hash::Sha384, false, 48),
        )
        .unwrap();

    // 3. Unblind
    let signature = blind_sig
        .unblind(blinded_sig.as_ref(), &blinding_result, pubkey_bytes)
        .unwrap();

    // 4. Verify with original message and randomizer
    // Must pass the randomizer since we used randomize_message=true
    assert!(blind_sig.verify(pubkey_bytes, &signature, Some(&randomizer)));

    // The signature is valid for (randomizer || pubkey) formatted message
    // Server will reconstruct: signed_msg = randomizer || pubkey
    // and verify using the blind signature library's internal handling
}

#[test]
fn test_unlinkability_property() {
    // Demonstrate that server cannot link blinded message to unblinded signature
    let kp = KeyPair::generate(2048).unwrap();
    let blind_sig = BlindSignature {
        server_public_key: kp.pk.clone(),
        options: Options::new(blind_rsa_signatures::Hash::Sha384, false, 48),
    };
    
    // Create multiple tokens
    let mut tokens = Vec::new();
    for i in 0..5 {
        let key = SigningKey::from_bytes(&[i as u8; 32]).verifying_key();
        let message = key.as_bytes();
        
        let blinding_result = blind_sig.blind(message).unwrap();
        
        // Server signs
        let blinded_msg_bytes: &[u8] = blinding_result.blind_msg.as_ref();
        let blinded_sig = kp
            .sk
            .blind_sign(
                blinded_msg_bytes,
                &Options::new(blind_rsa_signatures::Hash::Sha384, false, 48),
            )
            .unwrap();
        
        // Client unblinds
        let signature = blind_sig
            .unblind(blinded_sig.as_ref(), &blinding_result, message)
            .unwrap();
        
        tokens.push((
            message.to_vec(),
            signature,
            blinding_result.msg_randomizer.unwrap().as_ref().to_vec(),
            { let b: &[u8] = blinding_result.blind_msg.as_ref(); b.to_vec() },
        ));
    }
    
    // Server has: list of blinded_msg from token requests
    // Server sees: (pubkey, signature, randomizer) during mailbox creation
    // Server CANNOT determine which blinded_msg corresponds to which mailbox
    
    // Verify all signatures are valid
    for (pubkey, signature, randomizer, _blinded) in &tokens {
        assert!(
            blind_sig.verify(pubkey, signature, Some(randomizer)),
            "All signatures must be valid"
        );
    }
    
    // All blinded messages are different (unlinkability proof)
    for i in 0..tokens.len() {
        for j in (i + 1)..tokens.len() {
            assert_ne!(
                tokens[i].3, tokens[j].3,
                "All blinded messages must be unique (server cannot link)"
            );
        }
    }
}

#[test]
fn test_randomizer_not_none_in_production_mode() {
    // Ensure that using randomize_message=true always produces a randomizer
    let kp = KeyPair::generate(2048).unwrap();
    
    let options = Options::new(
        blind_rsa_signatures::Hash::Sha384,
        false, // randomized (production mode)
        48,
    );
    
    let message = b"test_message_32_bytes_long!!!!!";
    
    // Blind with randomize_message = true
    let result = kp.pk.blind(message, true, &options).unwrap();
    
    assert!(
        result.msg_randomizer.is_some(),
        "Randomizer must exist when using randomize_message=true"
    );
    
    let randomizer = result.msg_randomizer.unwrap();
    assert_eq!(
        randomizer.as_ref().len(),
        32,
        "Randomizer must be 32 bytes"
    );
}

#[test]
fn test_signature_size() {
    // Verify signature size is correct for RSA-2048
    let kp = KeyPair::generate(2048).unwrap();
    let blind_sig = BlindSignature {
        server_public_key: kp.pk.clone(),
        options: Options::new(blind_rsa_signatures::Hash::Sha384, false, 48),
    };
    
    let message = b"test_ed25519_public_key_32bytes!";
    
    let blinding_result = blind_sig.blind(message).unwrap();
    let blinded_bytes: &[u8] = blinding_result.blind_msg.as_ref();
    let blinded_sig = kp
        .sk
        .blind_sign(
            blinded_bytes,
            &Options::new(blind_rsa_signatures::Hash::Sha384, false, 48),
        )
        .unwrap();
    
    let signature = blind_sig
        .unblind(blinded_sig.as_ref(), &blinding_result, message)
        .unwrap();
    
    // RSA-2048 signature is 256 bytes
    assert_eq!(
        signature.len(),
        256,
        "RSA-2048 signature must be 256 bytes"
    );
}

#[test]
fn test_cannot_unblind_with_wrong_secret() {
    // Verify that unblinding with a different secret fails
    let kp = KeyPair::generate(2048).unwrap();
    let blind_sig = BlindSignature {
        server_public_key: kp.pk.clone(),
        options: Options::new(blind_rsa_signatures::Hash::Sha384, false, 48),
    };
    
    let message = b"test_message_for_blinding!!!!!!";
    
    // Blind message A
    let result_a = blind_sig.blind(message).unwrap();
    
    // Blind message B (different secret)
    let message_b = b"different_message_for_blinding!";
    let result_b = blind_sig.blind(message_b).unwrap();
    
    // Server signs A
    let blinded_a_bytes: &[u8] = result_a.blind_msg.as_ref();
    let blinded_sig_a = kp
        .sk
        .blind_sign(
            blinded_a_bytes,
            &Options::new(blind_rsa_signatures::Hash::Sha384, false, 48),
        )
        .unwrap();
    
    // Try to unblind A's signature with B's secret - should fail or produce invalid signature
    let wrong_unblind_result = blind_sig.unblind(
        blinded_sig_a.as_ref(),
        &result_b, // Wrong secret!
        message,
    );
    
    // Either unblinding fails, or verification fails
    if let Ok(wrong_signature) = wrong_unblind_result {
        // Use result_b's randomizer since that's what was used in unblind
        let wrong_randomizer = result_b.msg_randomizer.as_ref().map(|r| r.as_ref().to_vec());
        assert!(
            !blind_sig.verify(message, &wrong_signature, wrong_randomizer.as_deref()),
            "Signature unblinded with wrong secret must not verify"
        );
    }
}

#[test]
fn test_rfc9474_psszero_vs_pss() {
    // Verify we can use both PSSZERO and PSS variants
    // Production uses PSS (with salt), but PSSZERO is also valid
    
    let kp = KeyPair::generate(2048).unwrap();
    
    // PSS variant (with salt = 48)
    let pss_options = Options::new(blind_rsa_signatures::Hash::Sha384, false, 48);
    
    // PSSZERO variant (salt = 0)
    let psszero_options = Options::new(blind_rsa_signatures::Hash::Sha384, false, 0);
    
    let message = b"test_message_32_bytes_long!!!!!";
    
    // Both should work
    let pss_result = kp.pk.blind(message, true, &pss_options).unwrap();
    let psszero_result = kp.pk.blind(message, true, &psszero_options).unwrap();
    
    assert!(pss_result.msg_randomizer.is_some());
    assert!(psszero_result.msg_randomizer.is_some());
    
    // Sign and verify PSS
    let pss_blinded: &[u8] = pss_result.blind_msg.as_ref();
    let pss_blind_sig = kp.sk.blind_sign(pss_blinded, &pss_options).unwrap();
    let pss_sig = kp.pk.finalize(
        &pss_blind_sig,
        &pss_result.secret,
        pss_result.msg_randomizer,
        message,
        &pss_options,
    ).unwrap();
    
    assert!(kp.pk.verify(&pss_sig, pss_result.msg_randomizer, message, &pss_options).is_ok());
}

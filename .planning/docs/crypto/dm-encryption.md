# DM Encryption (Two-Layer)

DM messages use two layers of encryption for privacy and forward secrecy.

## Overview

```
┌─────────────────────────────────────────────────────────────────┐
│ LAYER 2: Mailbox Encryption                                     │
│ Purpose: SENDER ANONYMITY                                       │
│ Algorithm: XChaCha20-Poly1305                                   │
│ Key: X25519 shared secret (from DM bootstrap)                   │
│                                                                 │
│   ┌─────────────────────────────────────────────────────────┐   │
│   │ LAYER 1: Signal Protocol (Sealed Sender)                │   │
│   │ Purpose: FORWARD SECRECY + AUTHENTICATION               │   │
│   │ Algorithm: Double Ratchet + X25519 + AES-256-GCM        │   │
│   │                                                         │   │
│   │   ┌─────────────────────────────────────────────────┐   │   │
│   │   │ PLAINTEXT: Content proto                        │   │   │
│   │   │                                                 │   │   │
│   │   │ DataMessage { body: "Hello!", ... }             │   │   │
│   │   └─────────────────────────────────────────────────┘   │   │
│   └─────────────────────────────────────────────────────────┘   │
└─────────────────────────────────────────────────────────────────┘
```

## Why Two Layers?

| Layer | Purpose | What it Prevents |
|-------|---------|------------------|
| Signal (Inner) | Forward secrecy, authentication | Future key compromise reading past messages |
| Mailbox (Outer) | Sender anonymity | Server correlating sender/recipient |

## Send Flow

```rust
// In MessageService::send_dm_text()

// 1. Build content
let content = build_text_content(&text);
let plaintext = content.encode_to_vec();

// 2. Layer 1: Signal Protocol (sealed sender)
let signal_ciphertext = signal_service
    .encrypt_message(&mut auth_client, &peer_id, device_id, &plaintext)
    .await?;

// 3. Layer 2: Mailbox encryption
let shared_secret = conversation.shared_secret();
let mailbox_ciphertext = encrypt_dm_bytes(&signal_ciphertext, &shared_secret)?;

// 4. Send to anonymous mailbox
mailbox_manager.send_to_mailbox(&peer_id, &mailbox_ciphertext).await?;
```

## Receive Flow

```rust
// In MessageGateway::process_dm_message()

// 1. Layer 2: Decrypt mailbox layer
let signal_ciphertext = decrypt_dm_bytes(&mailbox_ciphertext, &shared_secret)?;

// 2. Layer 1: Decrypt Signal layer
let (sender_address, plaintext) = signal_service.decrypt_message(&signal_ciphertext).await?;

// 3. Validate sender certificate against TRUST_ROOT
// (Done internally by decrypt_message)

// 4. Parse content
let content = Content::decode(&plaintext)?;
```

## Mailbox Encryption (Layer 2)

**File**: `src/crypto/dm_crypto.rs`

```rust
/// Encrypt bytes for DM mailbox
pub fn encrypt_dm_bytes(plaintext: &[u8], shared_secret: &[u8; 32]) -> Result<Vec<u8>, CryptoError> {
    // Generate random nonce (24 bytes for XChaCha20)
    let nonce = XChaCha20Poly1305::generate_nonce(&mut OsRng);

    // Encrypt
    let cipher = XChaCha20Poly1305::new(shared_secret.into());
    let ciphertext = cipher.encrypt(&nonce, plaintext)?;

    // Prepend nonce
    let mut result = Vec::with_capacity(24 + ciphertext.len());
    result.extend_from_slice(&nonce);
    result.extend(ciphertext);
    Ok(result)
}

/// Decrypt bytes from DM mailbox
pub fn decrypt_dm_bytes(ciphertext: &[u8], shared_secret: &[u8; 32]) -> Result<Vec<u8>, CryptoError> {
    // Extract nonce (first 24 bytes)
    let (nonce_bytes, encrypted) = ciphertext.split_at(24);
    let nonce = XNonce::from_slice(nonce_bytes);

    // Decrypt
    let cipher = XChaCha20Poly1305::new(shared_secret.into());
    cipher.decrypt(nonce, encrypted).map_err(CryptoError::from)
}
```

## Shared Secret Establishment

During DM bootstrap:

```
Alice                                          Bob
  │                                             │
  ├─→ Generate X25519 keypair                   │
  │                                             │
  ├─→ Send KeyExchange message ─────────────────→│
  │   (our public key)                          │
  │                                             ├─→ Generate X25519 keypair
  │                                             │
  │←─────────────────── KeyExchange response ───┤
  │                    (their public key)       │
  │                                             │
  ├─→ Compute shared_secret = X25519(           │
  │       our_private_key,                      │
  │       their_public_key)                     │
  │                                             │
  └─→ Store in DmConversation                   │
```

## Signal Layer (Layer 1)

See [Signal Protocol](signal-protocol.md) for details.

Key points:
- Uses sealed sender for anonymity
- Double Ratchet provides forward secrecy
- Sender certificate validated against hardcoded trust root

## Security Properties

| Property | How Achieved |
|----------|--------------|
| Confidentiality | Both layers encrypt content |
| Forward Secrecy | Signal Double Ratchet |
| Sender Authentication | Sender certificate in sealed sender |
| Sender Anonymity | Server sees random mailbox, not recipient |
| Integrity | Both layers use authenticated encryption |

## Related Files

- `src/crypto/dm_crypto.rs` - Mailbox layer
- `src/crypto/signal/sealed_sender.rs` - Signal layer
- `src/mailbox/dm/bootstrap.rs` - Key exchange
- `src/mailbox/dm/conversation.rs` - Conversation state
- `src/services/message_gateway.rs` - Two-layer decryption

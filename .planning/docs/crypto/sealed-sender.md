# Sealed Sender

Anonymous messaging where the server cannot see the sender.

## Overview

Normal messages: Server sees `Alice → Bob`
Sealed sender: Server sees `??? → Bob`

The sender's identity is encrypted inside the message, only decryptable by the recipient.

## How It Works

```
┌─────────────────────────────────────────────────────────────────┐
│ SEALED SENDER ENVELOPE                                          │
│                                                                 │
│ Outer Layer (Server can see):                                   │
│   - Recipient's identity                                        │
│   - Encrypted blob                                              │
│                                                                 │
│ Inner Layer (Only recipient can decrypt):                       │
│   - Sender Certificate (proves sender identity)                 │
│   - Encrypted message content                                   │
└─────────────────────────────────────────────────────────────────┘
```

## Sender Certificate

Before sending sealed sender messages, obtain a certificate from server:

```rust
// Certificate contains:
pub struct SenderCertificate {
    pub sender_uuid: String,
    pub sender_device_id: u32,
    pub sender_key: PublicKey,  // Identity key
    pub expiration: u64,
    pub server_signature: Vec<u8>,  // Signed by TRUST_ROOT
}
```

### Certificate Flow

```
1. Client authenticates to server (knows who we are)
2. Client requests certificate: GET /v1/certificate/delivery
3. Server signs certificate with server private key
4. Client caches certificate (~24 hours validity)
5. When sending: embed certificate in sealed sender envelope
6. Recipient validates signature against TRUST_ROOT
```

## Trust Root

**CRITICAL**: Hardcoded in `src/config.rs`

```rust
pub const TRUST_ROOT_PUBLIC_KEY_BYTES: [u8; 32] = [
    0x6b, 0xb8, 0xd7, 0xac, 0xef, 0x0b, 0x47, 0xa2,
    // ... 32 bytes total
];
```

**Why hardcode?**
- Prevents MITM during registration
- Client trusts only this specific server key
- Never fetch from network

## Unidentified Access Key (UAK)

For sending to recipients who allow anonymous messages:

```rust
// UAK is 16 bytes, derived from recipient's profile key
pub const UAK_LENGTH: usize = 16;

// Recipient can configure:
// - Require UAK (only contacts with profile key can send)
// - Unrestricted (anyone can send sealed sender)
```

## Send Flow

```rust
// In SignalService::encrypt_message()

// 1. Get or refresh sender certificate
let certificate = get_sender_certificate(&mut auth_client).await?;

// 2. Ensure session exists
ensure_session(&mut auth_client, recipient_id, device_id).await?;

// 3. Encrypt with Double Ratchet
let inner_ciphertext = session.encrypt(plaintext)?;

// 4. Wrap in sealed sender envelope
let sealed = SealedSenderManager::encrypt(
    &certificate,
    recipient_identity_key,
    inner_ciphertext,
)?;
```

## Receive Flow

```rust
// In SignalService::decrypt_message()

// 1. Decrypt sealed sender envelope
let (sender_certificate, inner_ciphertext) = SealedSenderManager::decrypt(
    &sealed_ciphertext,
    &our_identity_key,
)?;

// 2. Validate certificate against TRUST_ROOT
validate_certificate(&sender_certificate, &TRUST_ROOT_PUBLIC_KEY)?;

// 3. Check certificate not expired
if sender_certificate.expiration < now() {
    return Err(CryptoError::CertificateExpired);
}

// 4. Decrypt inner message with Double Ratchet
let plaintext = session.decrypt(inner_ciphertext)?;

// 5. Return sender identity from certificate
Ok((sender_certificate.sender_address(), plaintext))
```

## Certificate Validation

```rust
fn validate_certificate(cert: &SenderCertificate, trust_root: &PublicKey) -> Result<()> {
    // 1. Extract signature and signed data
    let signature = &cert.server_signature;
    let signed_data = cert.to_signed_bytes();

    // 2. Verify with trust root public key
    if !trust_root.verify(signed_data, signature) {
        return Err(CryptoError::InvalidSenderCertificate(
            "Server signature validation failed".to_string()
        ));
    }

    // 3. Check expiration
    if cert.expiration < current_timestamp_ms() {
        return Err(CryptoError::InvalidSenderCertificate(
            "Certificate expired".to_string()
        ));
    }

    Ok(())
}
```

## Self-Send Prevention

```rust
// Cannot send sealed sender to yourself
if recipient_id == our_id {
    return Err(CryptoError::SealedSenderSelfSend);
}
```

## Error Types

```rust
pub enum CryptoError {
    /// Certificate signature doesn't match trust root
    InvalidSenderCertificate(String),

    /// Certificate has expired
    CertificateExpired,

    /// Attempted to send sealed sender to self
    SealedSenderSelfSend,

    /// Decryption failed
    SealedSenderDecryptionFailed(String),
}
```

## Privacy Properties

| What Server Sees | What Server Cannot See |
|------------------|----------------------|
| Recipient identity | Sender identity |
| Message timestamp | Message content |
| Message size (padded) | Who is talking to whom |

## Padding

Messages padded to bucket sizes to prevent size-based analysis:

```rust
// Pad to nearest bucket
const BUCKETS: [usize; 6] = [160, 320, 640, 1280, 2560, 5120];

fn pad_message(content: &[u8]) -> Vec<u8> {
    let target_size = BUCKETS.iter()
        .find(|&&s| s >= content.len())
        .unwrap_or(&5120);
    // Add random padding
}
```

## Related Files

- `src/crypto/signal/sealed_sender.rs` - Implementation
- `src/config.rs` - Trust root
- `src/services/signal_service.rs` - High-level API

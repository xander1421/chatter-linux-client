# Crypto Layer

Cryptographic primitives and Signal Protocol implementation.

## Overview

The crypto layer provides:
- Signal Protocol (PQXDH, Double Ratchet, Sealed Sender, Sender Keys)
- DM mailbox encryption (XChaCha20-Poly1305)
- Attachment encryption (future)

## Documentation

- [Signal Protocol](signal-protocol.md) - PQXDH, Double Ratchet overview
- [Sealed Sender](sealed-sender.md) - Anonymous messaging
- [Sender Keys](sender-keys.md) - Group encryption
- [DM Encryption](dm-encryption.md) - Two-layer mailbox encryption

## Key Principle

**Never call crypto directly from UI. Always use services.**

```rust
// ❌ WRONG - Direct crypto
let ciphertext = SealedSenderManager::encrypt(...).await?;

// ✅ CORRECT - Via SignalService
let ciphertext = signal_service.encrypt_message(...).await?;
```

## Files

```
crypto/
├── mod.rs
├── signal/
│   ├── mod.rs
│   ├── identity.rs        # IdentityKeyManager
│   ├── prekey.rs          # PreKeyManager
│   ├── session.rs         # SessionManager (PQXDH)
│   ├── sender_key.rs      # SenderKeyManager
│   ├── sealed_sender.rs   # SealedSenderManager
│   └── store.rs           # libsignal trait implementations
├── dm_crypto.rs           # Mailbox layer encryption
└── blind_signature.rs     # Chaum blind signatures
```

## Service Wrappers

| Crypto Component | Service Wrapper |
|-----------------|-----------------|
| SealedSenderManager | SignalService.encrypt_message() |
| SessionManager | SignalService.ensure_session() |
| SenderKeyManager | SignalService.encrypt_for_group() |
| DmCrypto | MessageService.send_dm_text() |

## Trust Root

**CRITICAL**: Hardcoded in `src/config.rs`

```rust
pub const TRUST_ROOT_PUBLIC_KEY_BYTES: [u8; 32] = [...];
```

Never fetch from server. This prevents MITM attacks.

## Encryption Layers

### DMs (Two Layers)

```
Plaintext
    │
    ▼
Signal Protocol (Sealed Sender)
  - Forward secrecy
  - Sender authentication
    │
    ▼
Mailbox Encryption (XChaCha20-Poly1305)
  - Sender anonymity
  - Server cannot correlate
```

### Channels (One Layer)

```
Plaintext
    │
    ▼
Sender Keys
  - O(1) encryption for group
  - All members can decrypt
```

## libsignal Integration

We implement libsignal's storage traits:

| Trait | Our Implementation |
|-------|-------------------|
| IdentityKeyStore | KeyStore |
| PreKeyStore | KeyStore |
| SignedPreKeyStore | KeyStore |
| KyberPreKeyStore | KeyStore |
| SessionStore | KeyStore |
| SenderKeyStore | KeyStore |

Located in `src/crypto/signal/store.rs`.

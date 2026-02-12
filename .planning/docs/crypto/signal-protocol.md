# Signal Protocol Overview

Core Signal Protocol implementation for end-to-end encryption.

## Components

| Component | Purpose | File |
|-----------|---------|------|
| PQXDH | Session establishment | `session.rs` |
| Double Ratchet | Message encryption | libsignal |
| Sealed Sender | Sender anonymity | `sealed_sender.rs` |
| Sender Keys | Group encryption | `sender_key.rs` |

## PQXDH (Post-Quantum Extended Diffie-Hellman)

Session establishment protocol combining X25519 and Kyber for post-quantum security.

### Key Types

| Key | Purpose | Algorithm |
|-----|---------|-----------|
| Identity Key | Long-term identity | Ed25519 |
| Signed PreKey | Medium-term, signed | X25519 |
| Kyber PreKey | Post-quantum security | ML-KEM 768 |
| One-Time PreKey | Single use | X25519 |

### Session Establishment

```
Alice wants to message Bob (no existing session):

1. Alice fetches Bob's PreKey Bundle:
   - Identity Key (IK_B)
   - Signed PreKey (SPK_B) + signature
   - Kyber PreKey (KPK_B)
   - One-Time PreKey (OPK_B) - optional

2. Alice generates ephemeral keys:
   - Ephemeral X25519 (EK_A)
   - Ephemeral Kyber (KEK_A)

3. Alice computes shared secrets:
   DH1 = X25519(IK_A, SPK_B)
   DH2 = X25519(EK_A, IK_B)
   DH3 = X25519(EK_A, SPK_B)
   DH4 = X25519(EK_A, OPK_B)  // if available
   KEM = Kyber.Decapsulate(KPK_B, KEK_A)

4. Alice derives session key:
   SK = KDF(DH1 || DH2 || DH3 || DH4 || KEM)

5. Alice sends initial message with:
   - Her Identity Key (IK_A)
   - Ephemeral Key (EK_A)
   - Kyber ciphertext
   - Which OPK was used

6. Bob reconstructs the same shared secrets
```

### In Code

```rust
// SignalService handles this automatically
signal_service.ensure_session(&mut auth_client, &recipient_id, device_id).await?;

// Or implicitly during encrypt
signal_service.encrypt_message(&mut auth_client, &recipient_id, device_id, &plaintext).await?;
```

## Double Ratchet

Provides forward secrecy through continuous key rotation.

### Properties

| Property | How Achieved |
|----------|--------------|
| Forward Secrecy | New keys per message, old keys deleted |
| Break-in Recovery | Compromise heals after new DH exchange |
| Out-of-Order | Message keys stored until used |

### Ratchet Steps

```
Each message:
1. Derive message key from chain key
2. Encrypt message
3. Advance chain key (delete old)
4. Periodically: new DH ratchet step
```

### In Code

```rust
// Double Ratchet is internal to libsignal
// We just call encrypt/decrypt, it handles ratcheting

let ciphertext = signal_service.encrypt_message(...).await?;
let plaintext = signal_service.decrypt_message(&ciphertext).await?;
```

## PreKey Management

### PreKey Counts

| Type | Count | Rotation |
|------|-------|----------|
| Signed PreKey | 1 active | Weekly |
| Kyber PreKey | 1 active | Weekly |
| One-Time PreKeys | 100 target | Replenish when low |

### Refresh Logic

```rust
// Check and refresh if needed
signal_service.refresh_prekeys_if_needed(&mut auth_client).await?;

// Or force refresh
signal_service.refresh_prekeys(&mut auth_client, 100).await?;
```

### Server Stats

```rust
let stats = auth_client.get_pre_key_stats(device_id).await?;
// stats.available_count - how many OTPKs left
```

## Trust Model

### Trust on First Use (TOFU)

```
First message from new contact:
1. Receive their Identity Key
2. Store it
3. Trust it (no verification yet)

Future messages:
1. Check Identity Key matches stored
2. If changed: WARN USER (possible MITM)
```

### Safety Numbers

```rust
// Generate safety number for verification
let safety_number = signal_service.get_safety_number(&peer_id)?;
// Users compare out-of-band (QR code, voice call)
```

## Key Storage

All keys stored in encrypted SQLCipher database via `KeyStore`:

| Table | Contents |
|-------|----------|
| `local_identity` | Our identity key pair |
| `remote_identities` | Peers' identity keys |
| `prekeys` | Our one-time prekeys |
| `signed_prekeys` | Our signed prekeys |
| `kyber_prekeys` | Our Kyber prekeys |
| `sessions` | Double Ratchet state |
| `sender_keys` | Group sender keys |

## libsignal Integration

We implement libsignal's store traits:

```rust
// In src/crypto/signal/store.rs
impl IdentityKeyStore for KeyStore { ... }
impl PreKeyStore for KeyStore { ... }
impl SignedPreKeyStore for KeyStore { ... }
impl KyberPreKeyStore for KeyStore { ... }
impl SessionStore for KeyStore { ... }
impl SenderKeyStore for KeyStore { ... }
```

## Related Documentation

- [Sealed Sender](sealed-sender.md) - Anonymous messaging
- [Sender Keys](sender-keys.md) - Group encryption
- [DM Encryption](dm-encryption.md) - Two-layer encryption
- [SIGNAL_PROTOCOL_AUDIT.md](../../SIGNAL_PROTOCOL_AUDIT.md) - Detailed verification

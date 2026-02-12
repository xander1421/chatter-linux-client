# Sender Keys (Group Encryption)

**Files**:
- `src/crypto/signal/sender_key.rs`
- `src/services/signal_service.rs` (high-level)

Efficient group encryption using Signal's Sender Keys protocol.

## Overview

Sender Keys provide O(1) encryption for groups:
- Each member has one sender key
- Message encrypted once, decryptable by all members with the key
- Much more efficient than N separate encryptions

```
Traditional approach (N encryptions):
  Message → Encrypt for Member1 → Send
  Message → Encrypt for Member2 → Send
  Message → Encrypt for Member3 → Send
  ...

Sender Keys (1 encryption):
  Message → Encrypt with SenderKey → Send to channel
  All members decrypt with stored sender key
```

## Sender Key Distribution Message (SKDM)

Before sending encrypted messages, distribute your sender key:

```rust
pub struct SenderKeyDistributionMessage {
    pub chain_id: u32,
    pub iteration: u32,
    pub chain_key: Vec<u8>,
    pub signing_key: Vec<u8>,
}
```

## Flow

### First Message to Channel

```
Alice sends first message to #general:

1. Check: Do I have a sender key for this channel?
   No → Create one

2. Create SenderKeyDistributionMessage
   │
   └─→ Send SKDM to all channel members
       (via device inbox, NOT encrypted with sender key)

3. Encrypt message with sender key
   │
   └─→ Send to channel
```

### Subsequent Messages

```
Alice sends another message:

1. Check: Do I have a sender key?
   Yes → Use it

2. Encrypt message
   │
   └─→ Send to channel
```

### Receiving Messages

```
Bob receives encrypted message from Alice:

1. Check: Do I have Alice's sender key?
   │
   ├─→ Yes → Decrypt message
   │
   └─→ No → Request sender key from Alice
            (SenderKeyRequest)
```

## API

### SignalService Methods

```rust
/// Encrypt for channel using sender key
pub async fn encrypt_for_group(
    &self,
    channel_id: &ChannelId,
    plaintext: &[u8],
) -> Result<(GroupEncrypted, Option<SenderKeyDistributionMessage>), ServiceError>

/// Decrypt sender key message
pub async fn decrypt_from_group(
    &self,
    sender: &ProtocolAddress,
    channel_id: &ChannelId,
    ciphertext: &[u8],
) -> Result<Vec<u8>, ServiceError>

/// Create new sender key for channel
pub fn create_sender_key(
    &self,
    channel_id: &ChannelId,
) -> Result<SenderKeyDistributionMessage, ServiceError>

/// Rotate sender key (after member kick)
pub fn rotate_sender_key(
    &self,
    channel_id: &ChannelId,
) -> Result<SenderKeyDistributionMessage, ServiceError>

/// Clear all sender keys for channel (on leave)
pub fn clear_channel_sender_keys(
    &self,
    channel_id: &ChannelId,
) -> Result<(), ServiceError>
```

## Sender Key Rotation

### When Member Leaves (Voluntary)

```rust
// In leave_channel()
signal_service.clear_channel_sender_keys(&channel_id)?;
// Leaver can't decrypt future messages (doesn't have new keys)
```

### When Member is Kicked

```rust
// In kick_user()
signal_service.rotate_sender_key(&channel_id)?;
// New SKDM distributed on next message
// Kicked user excluded from distribution
```

## Message Format

### EncryptedData (Outer Wrapper)

```protobuf
message EncryptedData {
    CryptoVersion version = 1;
    bytes nonce = 2;
    bytes ciphertext = 3;  // Contains GroupEncrypted
    bytes auth_tag = 4;
    bytes associated_data = 5;  // "sender_key" marker
    string device_id = 6;
}
```

### GroupEncrypted (Inner)

```protobuf
message GroupEncrypted {
    string channel_id = 1;
    uint32 sender_device_id = 2;
    bytes ciphertext = 3;  // Sender key encrypted
}
```

### Associated Data Markers

| Marker | Type | Handler |
|--------|------|---------|
| `b"sender_key"` | Encrypted content | `process_group_encrypted_message()` |
| `b"sender_key_distribution"` | SKDM | `process_sender_key_distribution()` |
| `b"sender_key_request"` | Key request | `process_sender_key_request()` |

## Storage

Sender keys stored in `sender_keys` table:

```sql
CREATE TABLE sender_keys (
    sender TEXT NOT NULL,          -- "user_id:device_id"
    distribution_id TEXT NOT NULL, -- channel_id
    record BLOB NOT NULL,          -- Serialized key
    updated_at TEXT NOT NULL,
    PRIMARY KEY (sender, distribution_id)
);
```

## Security Considerations

1. **Forward Secrecy**: Sender keys don't provide forward secrecy like Double Ratchet. If a sender key is compromised, all past messages encrypted with it can be decrypted.

2. **Key Rotation**: Rotate keys when members leave to maintain secrecy from former members.

3. **SKDM Distribution**: SKDMs sent via device inbox (not encrypted with sender key) to bootstrap new members.

## Related Files

- `src/crypto/signal/sender_key.rs` - Core implementation
- `src/services/signal_service.rs` - High-level API
- `src/services/message_gateway.rs` - Message routing
- `src/inbox/provider.rs` - SKDM/request handling

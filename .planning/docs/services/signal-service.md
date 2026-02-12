# SignalService

**File**: `src/services/signal_service.rs`

High-level Signal Protocol operations. Wraps all encryption/decryption.

## Purpose

SignalService is the **only entry point** for Signal Protocol operations. UI and other services must use this - never call crypto layer directly.

## Methods

### DM Encryption

```rust
/// Encrypt message for DM (sealed sender)
pub async fn encrypt_message(
    &self,
    auth_client: &mut AuthClient,
    recipient_id: &UserId,
    device_id: u32,
    plaintext: &[u8],
) -> Result<Vec<u8>, ServiceError>
```

What it does:
1. Ensures session exists (runs PQXDH if needed)
2. Gets sender certificate from server
3. Encrypts with sealed sender envelope
4. Returns ciphertext ready for mailbox layer

### DM Decryption

```rust
/// Decrypt sealed sender message
pub async fn decrypt_message(
    &self,
    ciphertext: &[u8],
) -> Result<(ProtocolAddress, Vec<u8>), ServiceError>
```

What it does:
1. Decrypts sealed sender envelope
2. **Validates sender certificate against TRUST_ROOT**
3. Returns sender address and plaintext

### Channel Encryption

```rust
/// Encrypt for channel using sender key
pub async fn encrypt_for_group(
    &self,
    channel_id: &ChannelId,
    plaintext: &[u8],
) -> Result<(GroupEncrypted, Option<SenderKeyDistributionMessage>), ServiceError>
```

What it does:
1. Gets or creates sender key for channel
2. Encrypts with sender key
3. Returns SKDM if this is first message (needs distribution)

### Channel Decryption

```rust
/// Decrypt sender key message
pub async fn decrypt_from_group(
    &self,
    sender: &ProtocolAddress,
    channel_id: &ChannelId,
    ciphertext: &[u8],
) -> Result<Vec<u8>, ServiceError>
```

### Session Management

```rust
/// Ensure session exists, establish via PQXDH if not
pub async fn ensure_session(
    &self,
    auth_client: &mut AuthClient,
    recipient_id: &UserId,
    device_id: u32,
) -> Result<(), ServiceError>
```

### Sender Key Management

```rust
/// Create new sender key for channel
pub fn create_sender_key(&self, channel_id: &ChannelId) -> Result<SenderKeyDistributionMessage, ServiceError>

/// Rotate sender key (after member kick)
pub fn rotate_sender_key(&self, channel_id: &ChannelId) -> Result<SenderKeyDistributionMessage, ServiceError>

/// Clear all sender keys for channel (on leave)
pub fn clear_channel_sender_keys(&self, channel_id: &ChannelId) -> Result<(), ServiceError>
```

### PreKey Management

```rust
/// Refresh prekeys on server
pub async fn refresh_prekeys(
    &self,
    auth_client: &mut AuthClient,
    target_count: u32,
) -> Result<u32, ServiceError>

/// Get identity fingerprint for display
pub fn get_identity_fingerprint(&self) -> Result<String, ServiceError>
```

## Usage Examples

### Send DM (in MessageService)

```rust
// MessageService delegates to SignalService
let ciphertext = signal_service
    .encrypt_message(&mut auth_client, &peer_id, device_id, &plaintext)
    .await?;
```

### Send Channel Message

```rust
let (group_encrypted, skdm_opt) = signal_service
    .encrypt_for_group(&channel_id, &plaintext)
    .await?;

// Distribute SKDM if returned
if let Some(skdm) = skdm_opt {
    distribute_sender_key(skdm, &channel_id).await?;
}
```

### Settings - Fingerprint (UI)

```rust
// In settings.rs - UI correctly delegates
let fingerprint = signal_service.get_identity_fingerprint()?;
```

## What NOT to Do

```rust
// ❌ WRONG - Direct crypto access
use crate::crypto::signal::SealedSenderManager;
let ciphertext = SealedSenderManager::encrypt(...).await?;

// ✅ CORRECT - Use SignalService
let ciphertext = signal_service.encrypt_message(...).await?;
```

## Dependencies

- `KeyStore` - Signal key persistence
- `SettingsRepository` - User ID, device ID
- libsignal crate - Underlying crypto

## Trust Root

SignalService validates sender certificates against the hardcoded trust root in `src/config.rs`. This prevents MITM attacks - certificates signed by any other key are rejected.

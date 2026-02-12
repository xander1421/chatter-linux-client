# Key Store

Signal Protocol key storage with libsignal trait implementations.

## Overview

`KeyStore` provides persistent storage for all Signal Protocol keys:
- Identity key pairs
- PreKeys (one-time, signed, Kyber)
- Sessions (Double Ratchet state)
- Sender keys (group encryption)

## File

`src/storage/key_store.rs`

## Structure

```rust
pub struct KeyStore {
    db: Arc<Database>,
}
```

## libsignal Trait Implementations

KeyStore implements all required libsignal storage traits:

| Trait | Purpose | Table |
|-------|---------|-------|
| `IdentityKeyStore` | Identity key management | `local_identity`, `remote_identities` |
| `PreKeyStore` | One-time prekeys | `prekeys` |
| `SignedPreKeyStore` | Signed prekeys | `signed_prekeys` |
| `KyberPreKeyStore` | Post-quantum prekeys | `kyber_prekeys` |
| `SessionStore` | Double Ratchet sessions | `sessions` |
| `SenderKeyStore` | Group sender keys | `sender_keys` |

## Identity Key Management

### Store Local Identity

```rust
impl IdentityKeyStore for KeyStore {
    async fn save_identity(
        &mut self,
        address: &ProtocolAddress,
        identity: &IdentityKey,
        _ctx: Context,
    ) -> Result<bool, SignalProtocolError> {
        // Insert or update remote identity
        self.db.execute(
            "INSERT INTO remote_identities (address, identity_key, first_seen_at, updated_at)
             VALUES (?1, ?2, ?3, ?3)
             ON CONFLICT(address) DO UPDATE SET
                identity_key = excluded.identity_key,
                updated_at = excluded.updated_at",
            params![address.to_string(), identity.serialize(), now()],
        )?;

        Ok(true)  // true = identity changed
    }

    async fn get_identity(
        &self,
        address: &ProtocolAddress,
        _ctx: Context,
    ) -> Result<Option<IdentityKey>, SignalProtocolError> {
        let key_bytes: Option<Vec<u8>> = self.db.query_row(
            "SELECT identity_key FROM remote_identities WHERE address = ?1",
            params![address.to_string()],
            |row| row.get(0),
        ).optional()?;

        match key_bytes {
            Some(bytes) => Ok(Some(IdentityKey::decode(&bytes)?)),
            None => Ok(None),
        }
    }
}
```

### Trust Decision

```rust
async fn is_trusted_identity(
    &self,
    address: &ProtocolAddress,
    identity: &IdentityKey,
    direction: Direction,
    _ctx: Context,
) -> Result<bool, SignalProtocolError> {
    match self.get_identity(address, _ctx).await? {
        None => {
            // First contact - trust on first use (TOFU)
            Ok(true)
        }
        Some(stored) => {
            // Check if identity matches
            if stored == *identity {
                Ok(true)
            } else {
                // Identity changed! This is a WARNING condition
                // In production: prompt user to verify
                tracing::warn!(
                    "Identity key changed for {}! Possible MITM attack.",
                    address
                );
                Ok(false)  // Reject until user verifies
            }
        }
    }
}
```

## Session Management

```rust
impl SessionStore for KeyStore {
    async fn load_session(
        &self,
        address: &ProtocolAddress,
        _ctx: Context,
    ) -> Result<Option<SessionRecord>, SignalProtocolError> {
        let record_bytes: Option<Vec<u8>> = self.db.query_row(
            "SELECT record FROM sessions WHERE address = ?1",
            params![address.to_string()],
            |row| row.get(0),
        ).optional()?;

        match record_bytes {
            Some(bytes) => Ok(Some(SessionRecord::deserialize(&bytes)?)),
            None => Ok(None),
        }
    }

    async fn store_session(
        &mut self,
        address: &ProtocolAddress,
        record: &SessionRecord,
        _ctx: Context,
    ) -> Result<(), SignalProtocolError> {
        self.db.execute(
            "INSERT INTO sessions (address, record, updated_at)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(address) DO UPDATE SET
                record = excluded.record,
                updated_at = excluded.updated_at",
            params![address.to_string(), record.serialize()?, now()],
        )?;
        Ok(())
    }
}
```

## Sender Key Storage

```rust
impl SenderKeyStore for KeyStore {
    async fn store_sender_key(
        &mut self,
        sender: &ProtocolAddress,
        distribution_id: Uuid,
        record: &SenderKeyRecord,
        _ctx: Context,
    ) -> Result<(), SignalProtocolError> {
        self.db.execute(
            "INSERT INTO sender_keys (sender, distribution_id, record, updated_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(sender, distribution_id) DO UPDATE SET
                record = excluded.record,
                updated_at = excluded.updated_at",
            params![
                sender.to_string(),
                distribution_id.to_string(),
                record.serialize()?,
                now()
            ],
        )?;
        Ok(())
    }

    async fn load_sender_key(
        &self,
        sender: &ProtocolAddress,
        distribution_id: Uuid,
        _ctx: Context,
    ) -> Result<Option<SenderKeyRecord>, SignalProtocolError> {
        // Query and deserialize
    }
}
```

## PreKey Management

```rust
impl PreKeyStore for KeyStore {
    async fn get_pre_key(
        &self,
        id: PreKeyId,
        _ctx: Context,
    ) -> Result<PreKeyRecord, SignalProtocolError> {
        let record_bytes: Vec<u8> = self.db.query_row(
            "SELECT record FROM prekeys WHERE id = ?1",
            params![id.to_string()],
            |row| row.get(0),
        )?;
        PreKeyRecord::deserialize(&record_bytes)
    }

    async fn save_pre_key(
        &mut self,
        id: PreKeyId,
        record: &PreKeyRecord,
        _ctx: Context,
    ) -> Result<(), SignalProtocolError> {
        self.db.execute(
            "INSERT INTO prekeys (id, record, created_at)
             VALUES (?1, ?2, ?3)",
            params![id.to_string(), record.serialize()?, now()],
        )?;
        Ok(())
    }

    async fn remove_pre_key(
        &mut self,
        id: PreKeyId,
        _ctx: Context,
    ) -> Result<(), SignalProtocolError> {
        // Called after prekey is used (single-use)
        self.db.execute(
            "DELETE FROM prekeys WHERE id = ?1",
            params![id.to_string()],
        )?;
        Ok(())
    }
}
```

## High-Level Methods

Beyond libsignal traits, KeyStore provides helper methods:

```rust
impl KeyStore {
    /// Get our identity key pair
    pub fn get_local_identity(&self) -> Result<IdentityKeyPair, StorageError>

    /// Store our identity key pair
    pub fn store_local_identity(
        &self,
        registration_id: u32,
        identity_pair: &IdentityKeyPair,
    ) -> Result<(), StorageError>

    /// Get registration ID
    pub fn get_registration_id(&self) -> Result<u32, StorageError>

    /// Count available prekeys
    pub fn count_prekeys(&self) -> Result<u32, StorageError>

    /// Clear all sender keys for a channel (on leave)
    pub fn clear_sender_keys_for_channel(
        &self,
        channel_id: &ChannelId,
    ) -> Result<(), StorageError>
}
```

## Usage

KeyStore is typically accessed through SignalService:

```rust
// ❌ WRONG - Direct access from UI
let identity = ctx.key_store.get_local_identity()?;

// ✅ CORRECT - Via SignalService
let fingerprint = ctx.signal_service.get_identity_fingerprint()?;
```

## Related Files

- `src/storage/key_store.rs` - Implementation
- `src/crypto/signal/store.rs` - Trait implementations
- `src/services/signal_service.rs` - High-level wrapper

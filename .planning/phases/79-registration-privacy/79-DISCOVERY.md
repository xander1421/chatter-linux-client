# Phase 79: Registration Privacy - Discovery

## Audit Findings

Security audit comparing Chatter registration with Signal Protocol implementation revealed critical privacy leaks.

### Signal Reference Files Analyzed

**Signal-Desktop:**
- `/home/alex/Documents/Signal-Desktop/ts/textsecure/AccountManager.preload.ts` - Registration flow
- `/home/alex/Documents/Signal-Desktop/ts/textsecure/WebAPI.preload.ts:2859` - Account creation request

**Signal-Server:**
- `/home/alex/Documents/Signal-server/service/src/main/java/org/whispersystems/textsecuregcm/controllers/RegistrationController.java`
- `/home/alex/Documents/Signal-server/service/src/main/java/org/whispersystems/textsecuregcm/storage/Account.java`
- `/home/alex/Documents/Signal-server/service/src/main/java/org/whispersystems/textsecuregcm/auth/UnidentifiedAccessUtil.java`

---

## Critical Issues (Fix in This Phase)

### 1. Profile Key Sent to Server

**Location:** `src/services/auth_service.rs:376`, `src/network/grpc/auth_client.rs:191`

**Current behavior:**
```rust
profile_key: profile_key.to_vec(),  // 32 bytes sent directly to server
```

**Signal's approach:**
- Profile key NEVER leaves client
- Only `unidentified_access_key` (16 bytes) derived via AES-GCM sent to server
- Derivation: `AES-GCM(profile_key, zero_nonce, empty_data)` → 16-byte tag

**Fix required:**
- Stop sending `profile_key` in RegisterRequest
- Derive UAK client-side and send only that
- Update proto if needed (remove profile_key field or make optional)

### 2. Plaintext Display Name

**Location:** `src/services/auth_service.rs:373`, `src/network/grpc/auth_client.rs:186`

**Current behavior:**
```rust
display_name: display_name.to_string(),  // Plaintext to server
```

**Signal's approach:**
- Display name encrypted with profile key before upload
- Server stores encrypted blob, cannot read content
- Uses padding to prevent length-based inference

**Fix required:**
- Encrypt display_name with profile key before sending
- Add padding to fixed length (Signal uses 128 bytes)
- Update profile update flows to also encrypt

### 3. Friend Request Messages Unencrypted

**Location:** `src/services/user/mod.rs:237-245`

**Current behavior:**
```rust
user_client.send_friend_request(&friend_id_str, message).await?;
// message is Option<&str> - plaintext
```

**Signal's approach:**
- All messages use E2E encryption
- Initial contact uses sealed sender
- Server sees only encrypted envelope

**Fix required:**
- Establish Signal session before sending friend request
- Encrypt friend request message with recipient's prekey bundle
- Server should only see encrypted content

---

## High-Risk Issues (Document for Future Phase)

### 4. No Username Hashing
- Usernames stored and searchable in plaintext
- Signal uses 32-byte SHA256 hashes for lookup
- **Requires server changes** - document only

### 5. Social Graph Visible to Server
- Friend lists, block lists fully visible
- Signal uses Private Contact Discovery (CDSI)
- **Requires major architectural change** - document only

### 6. No ACI/PNI Dual Identity
- Single user_id tied to username
- Signal separates Account Identity from Phone Identity
- **Requires server redesign** - document only

### 7. No Phone Number Discoverability Toggle
- All users always discoverable
- Signal allows opt-out from contact discovery
- **Requires server changes** - document only

---

## Implementation Order

1. **79-01-PLAN**: Stop sending profile_key, derive UAK client-side
2. **79-02-PLAN**: Encrypt display_name with profile key
3. **79-03-PLAN**: Encrypt friend request messages

---

## Technical Details

### UAK Derivation (from Signal)

```rust
// From device_credentials.rs - verify this matches Signal
fn derive_uak(profile_key: &[u8; 32]) -> [u8; 16] {
    use aes_gcm::{Aes256Gcm, Nonce, KeyInit};
    use aes_gcm::aead::Aead;

    let cipher = Aes256Gcm::new_from_slice(profile_key).unwrap();
    let nonce = Nonce::from_slice(&[0u8; 12]); // zero nonce

    // Empty plaintext, extract authentication tag
    let ciphertext = cipher.encrypt(nonce, &[0u8; 0] as &[u8]).unwrap();

    // Tag is last 16 bytes
    let mut uak = [0u8; 16];
    uak.copy_from_slice(&ciphertext[ciphertext.len()-16..]);
    uak
}
```

### Profile Name Encryption (from Signal)

```typescript
// From Signal-Desktop encryptProfileData.preload.ts
function encryptProfileItemWithPadding(
  data: Uint8Array,
  key: Uint8Array,
  paddedLength: number
): Uint8Array {
  const padded = new Uint8Array(paddedLength);
  padded.set(data);
  // AES-GCM encrypt with profile key
  return encrypt(key, padded);
}

// PaddedLengths.Name = 128 (from Signal)
```

### Friend Request Encryption

Need to:
1. Fetch recipient's prekey bundle via `get_pre_key_bundle()`
2. Establish X3DH session
3. Encrypt message content using Double Ratchet
4. Send encrypted envelope instead of plaintext

---

## Proto Changes Required

### Option A: Modify existing proto (requires server update)
```protobuf
message RegisterRequest {
  // Remove: bytes profile_key = X;
  bytes unidentified_access_key = Y; // 16 bytes only
  bytes encrypted_display_name = Z;  // AES-GCM encrypted
}
```

### Option B: Client-only changes (no server update)
- Continue sending profile_key but derive UAK locally
- Server may ignore profile_key if UAK provided
- Still improves client privacy posture
- Less impactful but incomplete fix

**Decision needed:** Does server need updating?

---

## References

- Signal Protocol Specification: https://signal.org/docs/
- NDSS '21 Paper: Sealed Sender implementation
- libsignal-protocol-rust: UAK derivation examples

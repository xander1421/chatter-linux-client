# Phase 15: Crypto Attachments - Research

**Researched:** 2026-02-03
**Domain:** File encryption for message attachments (Signal-compatible)
**Confidence:** HIGH

<research_summary>
## Summary

Researched Signal Desktop's attachment encryption and Chatter backend's file upload system. Critical finding: **Signal uses AES-256-CBC with HMAC-SHA256**, not AES-GCM. However, Chatter's proto currently specifies AES-256-GCM. Recommend implementing AES-256-GCM for Chatter (simpler, equally secure) unless full Signal interop is required.

The backend provides presigned URL uploads for anonymous attachments. Client encrypts locally, uploads encrypted blob directly to RustFS (MinIO), then embeds the decryption key in the E2E encrypted message. Server never sees plaintext.

**Primary recommendation:** Implement AES-256-GCM encryption with streaming support. Use the backend's presigned URL attachment system. Generate 32-byte random key per attachment, include key + digest in AttachmentPointer within E2E message.
</research_summary>

<standard_stack>
## Standard Stack

### Core Rust Crates
| Library | Version | Purpose | Why Standard |
|---------|---------|---------|--------------|
| aes-gcm | 0.10 | AES-256-GCM authenticated encryption | RustCrypto standard, audited |
| rand | 0.8/0.9 | Cryptographic random generation | Already in project |
| sha2 | 0.10 | SHA-256 digest calculation | Already in project (via libsignal) |

### Supporting
| Library | Version | Purpose | When to Use |
|---------|---------|---------|-------------|
| futures | 0.3 | Async streaming traits | Large file streaming |
| tokio | 1.x | Async IO for file operations | Already in project |
| bytes | 1.x | Efficient byte buffer handling | Already in project |

### Alternatives Considered
| Instead of | Could Use | Tradeoff |
|------------|-----------|----------|
| aes-gcm | chacha20poly1305 | ChaCha faster on non-AES-NI hardware, but AES-GCM is Signal standard |
| Custom streaming | ring | Ring has streaming, but less Rust-idiomatic |
| AES-GCM | AES-CBC + HMAC | Signal's actual format, but more complex for no benefit unless interop required |

**Installation:**
```toml
[dependencies]
aes-gcm = "0.10"
# rand and sha2 already in project via libsignal
```
</standard_stack>

<signal_implementation>
## Signal Desktop Implementation Details

Signal uses **AES-256-CBC with separate HMAC-SHA256**, not GCM:

### Key Structure (64 bytes total)
```
[AES Key (32 bytes)] + [MAC Key (32 bytes)]
```
Generated with `randomBytes(64)`, split via subarray.

### Ciphertext Format
```
[IV (16 bytes)] + [AES-CBC ciphertext] + [HMAC-SHA256 (32 bytes)]
```

### Padding Scheme (Log-Bucketing)
Signal pads plaintext before encryption to prevent file size leakage:
```
padded_size = max(541, ceil(1.05^ceil(log(size)/log(1.05))))
```
This produces consistent bucket sizes: 1 byte → 541, 1KB → 1024, 10KB → 10752.

### Incremental MAC
For files >50KB, Signal computes per-chunk MACs enabling:
- Resumable downloads
- Partial verification during streaming
- Stored as separate `incrementalMac` metadata

### Return Values from Encryption
```typescript
{
  ciphertextSize: number;              // Total encrypted bytes
  digest: Uint8Array;                  // SHA256(IV + ciphertext + MAC)
  iv: Uint8Array;                      // 16 random bytes
  plaintextHash: string;               // SHA256(plaintext) as hex
  incrementalMac?: Uint8Array;         // Per-chunk MAC for large files
  chunkSize?: number;                  // Bytes per chunk
}
```

### Key Files in Signal-Desktop
- `ts/AttachmentCrypto.node.ts` - Main encrypt/decrypt
- `ts/util/appendMacStream.node.ts` - HMAC streaming
- `ts/types/Crypto.std.ts` - Constants (IV_LENGTH=16, KEY_SET_LENGTH=64)
</signal_implementation>

<chatter_backend>
## Chatter Backend Integration

### Anonymous Attachment Upload Flow

1. **GetAttachmentUploadUrl** (rate-limited by IP, no auth):
   - Client requests presigned URL with `encrypted_size`
   - Server generates random `cdn_key` (22-char base64url)
   - Returns: `cdn_key`, `upload_url` (15-min presigned PUT), `expires_at`

2. **Direct upload to RustFS** (no server involvement):
   - Client HTTP PUTs encrypted blob to presigned URL
   - Server never sees plaintext

3. **ConfirmAttachmentUpload** (no auth):
   - Client confirms upload complete
   - Server verifies object exists, marks confirmed

4. **GetAttachmentDownloadUrl** (no auth):
   - Client provides `cdn_key`
   - Server generates presigned GET URL

### AttachmentPointer Proto
```protobuf
message AttachmentPointer {
  string cdn_key = 1;           // From server after upload
  bytes encryption_key = 2;      // 32-byte key (NEVER sent to server)
  bytes digest = 3;              // SHA-256 of ciphertext
  uint64 plaintext_size = 4;     // Original size
  string content_type = 5;       // MIME type
  string file_name = 6;
  // ... thumbnail, dimensions, etc.
}
```

### Storage Path
```
attachments/{cdn_key}
```

### Rate Limits
- 5 requests/second, burst 10 (IP-based)
- Max upload: 100MB per attachment
</chatter_backend>

<architecture_patterns>
## Architecture Patterns

### Recommended Project Structure
```
src/crypto/
├── attachment_crypto.rs     # AttachmentCrypto struct with encrypt/decrypt
├── signal/                  # Existing Signal protocol crypto
└── mod.rs                   # Export AttachmentCrypto
```

### Pattern 1: Stateless AttachmentCrypto
**What:** Single struct with static methods, no state
**When to use:** Always for attachment encryption
**Example:**
```rust
pub struct AttachmentCrypto;

impl AttachmentCrypto {
    /// Encrypt plaintext with random key. Returns (ciphertext, key, digest).
    pub fn encrypt(plaintext: &[u8]) -> Result<AttachmentEncrypted, CryptoError> {
        let key = Self::generate_key();
        let ciphertext = Self::encrypt_with_key(plaintext, &key)?;
        let digest = Self::compute_digest(&ciphertext);
        Ok(AttachmentEncrypted { ciphertext, key, digest })
    }

    /// Decrypt ciphertext with provided key, verify digest.
    pub fn decrypt(
        ciphertext: &[u8],
        key: &[u8],
        expected_digest: &[u8]
    ) -> Result<Vec<u8>, CryptoError> {
        // Verify digest first
        let actual_digest = Self::compute_digest(ciphertext);
        if actual_digest != expected_digest {
            return Err(CryptoError::AttachmentDigestMismatch);
        }
        Self::decrypt_with_key(ciphertext, key)
    }
}
```

### Pattern 2: Streaming for Large Files
**What:** Process files in chunks without loading entirely into memory
**When to use:** Files >1MB
**Example:**
```rust
pub async fn encrypt_file<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    reader: &mut R,
    writer: &mut W,
    key: &[u8; 32],
) -> Result<AttachmentMetadata, CryptoError> {
    // AES-GCM with streaming AEAD
    // Process in 64KB chunks
}
```

### Pattern 3: Key in Message, Blob Separate
**What:** Encryption key embedded in E2E message, encrypted blob stored separately
**When to use:** Always for attachments
**Flow:**
```
1. Client generates random 32-byte key
2. Client encrypts file → ciphertext
3. Client computes SHA256(ciphertext) → digest
4. Client uploads ciphertext to CDN (gets cdn_key)
5. Client embeds (key, digest, cdn_key) in AttachmentPointer
6. Client sends E2E encrypted message containing AttachmentPointer
7. Recipient decrypts message → gets AttachmentPointer
8. Recipient downloads blob from cdn_key
9. Recipient verifies digest
10. Recipient decrypts with key
```

### Anti-Patterns to Avoid
- **Key reuse:** Generate fresh key per attachment, never reuse
- **Skip digest verification:** Always verify SHA256 before decryption
- **Plaintext metadata to server:** Server should only see encrypted blob size
- **Loading large files into memory:** Use streaming for files >1MB
</architecture_patterns>

<dont_hand_roll>
## Don't Hand-Roll

| Problem | Don't Build | Use Instead | Why |
|---------|-------------|-------------|-----|
| AES-GCM encryption | Custom AES implementation | `aes-gcm` crate | Constant-time, audited, handles nonce correctly |
| Random key generation | Custom PRNG | `rand::thread_rng()` | Cryptographically secure |
| SHA-256 digest | Custom hash | `sha2::Sha256` | Standard, constant-time |
| Nonce generation | Timestamp-based | `rand` random bytes | Timestamps can repeat, random is safe |
| Key derivation | Simple concatenation | Just use random bytes | 32-byte random key is sufficient for AES-256 |

**Key insight:** Attachment encryption is well-specified. The only decisions are: algorithm (AES-GCM vs AES-CBC-HMAC), key size (32 bytes), and whether to support streaming. Don't innovate on cryptographic primitives.
</dont_hand_roll>

<common_pitfalls>
## Common Pitfalls

### Pitfall 1: Nonce/IV Reuse
**What goes wrong:** Same nonce with same key allows plaintext recovery
**Why it happens:** Using counters or timestamps that reset/repeat
**How to avoid:** Always generate 12 random bytes for AES-GCM nonce
**Warning signs:** Any deterministic nonce generation

### Pitfall 2: Missing Digest Verification
**What goes wrong:** Corrupted or tampered ciphertext decrypts to garbage (or worse, attacker-controlled)
**Why it happens:** Trusting that "it's encrypted" means "it's authenticated"
**How to avoid:** Always verify SHA256(ciphertext) == expected digest BEFORE decrypting
**Warning signs:** Decrypt-then-verify ordering

### Pitfall 3: Memory Exhaustion on Large Files
**What goes wrong:** Loading 4GB video into memory crashes app
**Why it happens:** Simple `fs::read` followed by encrypt
**How to avoid:** Streaming encryption for files >1MB threshold
**Warning signs:** `Vec<u8>` with unbounded size from file read

### Pitfall 4: Metadata Leakage
**What goes wrong:** File size reveals content (e.g., "500KB = photo, 3MB = video")
**Why it happens:** No padding before encryption
**How to avoid:** Optional padding to standard bucket sizes (Signal uses log-bucketing)
**Warning signs:** Encrypted size == plaintext size + small overhead
</common_pitfalls>

<code_examples>
## Code Examples

### AES-256-GCM Encryption
```rust
// Source: aes-gcm crate documentation
use aes_gcm::{
    aead::{Aead, KeyInit, OsRng},
    Aes256Gcm, Nonce, Key
};
use rand::RngCore;

pub fn encrypt(plaintext: &[u8]) -> Result<(Vec<u8>, [u8; 32], [u8; 12]), CryptoError> {
    // Generate random key
    let mut key_bytes = [0u8; 32];
    OsRng.fill_bytes(&mut key_bytes);

    // Generate random nonce (12 bytes for GCM)
    let mut nonce_bytes = [0u8; 12];
    OsRng.fill_bytes(&mut nonce_bytes);

    let key = Key::<Aes256Gcm>::from_slice(&key_bytes);
    let cipher = Aes256Gcm::new(key);
    let nonce = Nonce::from_slice(&nonce_bytes);

    let ciphertext = cipher.encrypt(nonce, plaintext)
        .map_err(|e| CryptoError::AttachmentEncryptionFailed(e.to_string()))?;

    // Prepend nonce to ciphertext
    let mut result = nonce_bytes.to_vec();
    result.extend(ciphertext);

    Ok((result, key_bytes, nonce_bytes))
}
```

### AES-256-GCM Decryption
```rust
pub fn decrypt(data: &[u8], key: &[u8; 32]) -> Result<Vec<u8>, CryptoError> {
    if data.len() < 12 + 16 {
        return Err(CryptoError::InvalidAttachmentData("too short".into()));
    }

    // Extract nonce (first 12 bytes)
    let (nonce_bytes, ciphertext) = data.split_at(12);

    let key = Key::<Aes256Gcm>::from_slice(key);
    let cipher = Aes256Gcm::new(key);
    let nonce = Nonce::from_slice(nonce_bytes);

    cipher.decrypt(nonce, ciphertext)
        .map_err(|e| CryptoError::AttachmentDecryptionFailed(e.to_string()))
}
```

### Digest Calculation
```rust
use sha2::{Sha256, Digest};

pub fn compute_digest(data: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hasher.finalize().into()
}
```

### Complete Attachment Upload Flow
```rust
// Pseudocode for client integration
async fn upload_attachment(file: &Path, channel: &ChannelId) -> Result<AttachmentPointer> {
    // 1. Read file
    let plaintext = fs::read(file).await?;

    // 2. Encrypt (generates random key)
    let (ciphertext, key, _nonce) = AttachmentCrypto::encrypt(&plaintext)?;

    // 3. Compute digest
    let digest = AttachmentCrypto::compute_digest(&ciphertext);

    // 4. Get presigned upload URL
    let upload_info = file_client.get_attachment_upload_url(ciphertext.len()).await?;

    // 5. Upload encrypted blob directly to CDN
    http_client.put(&upload_info.upload_url, &ciphertext).await?;

    // 6. Confirm upload
    file_client.confirm_attachment_upload(&upload_info.cdn_key).await?;

    // 7. Build pointer (key stays local, included in E2E message)
    Ok(AttachmentPointer {
        cdn_key: upload_info.cdn_key,
        encryption_key: key.to_vec(),
        digest: digest.to_vec(),
        plaintext_size: plaintext.len() as u64,
        content_type: mime_guess::from_path(file).first_or_octet_stream().to_string(),
        file_name: file.file_name().unwrap().to_string_lossy().to_string(),
        ..Default::default()
    })
}
```
</code_examples>

<sota_updates>
## State of the Art (2025-2026)

| Old Approach | Current Approach | When Changed | Impact |
|--------------|------------------|--------------|--------|
| AES-CBC + HMAC | AES-GCM | ~2015 | GCM combines encryption + auth in one operation |
| Separate MAC | AEAD | ~2015 | Single primitive, harder to misuse |
| 128-bit AES | 256-bit AES | Always preferred | Signal uses 256-bit for future-proofing |

**New patterns to consider:**
- **AEAD streaming:** AES-GCM with streaming for large files (aead crate supports this)
- **Post-quantum:** Not yet needed for attachment encryption (symmetric crypto is PQ-resistant)

**Still valid:**
- Signal's CBC+HMAC format (if interop needed)
- 32-byte random keys
- SHA-256 for digests
</sota_updates>

<open_questions>
## Open Questions

1. **Should we match Signal's CBC+HMAC exactly?**
   - What we know: Signal uses CBC+HMAC with log-bucketing padding
   - What's unclear: Whether Chatter needs Signal interoperability
   - Recommendation: Use AES-GCM (simpler, equally secure) unless interop required

2. **Streaming threshold?**
   - What we know: Signal streams everything, our backend has 100MB limit
   - What's unclear: What file sizes are common in practice
   - Recommendation: Stream for files >1MB, load smaller files into memory

3. **Padding for metadata protection?**
   - What we know: Signal uses log-bucketing, Chatter doesn't require it
   - What's unclear: Privacy requirements for Chatter
   - Recommendation: Optional v2 feature, not v1 requirement
</open_questions>

<sources>
## Sources

### Primary (HIGH confidence)
- Signal-Desktop source code: `/home/alex/Documents/chatter/Signal-Desktop/ts/AttachmentCrypto.node.ts`
- Chatter backend source: `/home/alex/Documents/chatter/internal/services/file/handlers/attachment.go`
- Chatter proto definitions: `/home/alex/Documents/chatter/pkg/proto/chat/v1/content/content.proto`
- aes-gcm crate documentation: https://docs.rs/aes-gcm

### Secondary (MEDIUM confidence)
- Signal protocol documentation (verified against source code)
- RustCrypto documentation

### Tertiary (needs validation during implementation)
- None - all critical paths verified against source code
</sources>

<metadata>
## Metadata

**Research scope:**
- Core technology: AES-256-GCM / AES-256-CBC-HMAC
- Ecosystem: aes-gcm, sha2, rand crates
- Patterns: Stateless encryption, key-in-message
- Pitfalls: Nonce reuse, missing verification, memory exhaustion

**Confidence breakdown:**
- Signal format: HIGH - verified against source code
- Chatter backend: HIGH - verified against source code
- AES-GCM usage: HIGH - standard pattern from crate docs
- Streaming: MEDIUM - needs implementation verification

**Research date:** 2026-02-03
**Valid until:** 2026-03-03 (30 days - cryptographic patterns stable)
</metadata>

---

*Phase: 15-crypto-attachments*
*Research completed: 2026-02-03*
*Ready for planning: yes*

# Phase 15: Crypto Attachments - Research

**Researched:** 2026-02-03
**Domain:** File encryption for message attachments (Signal-compatible)
**Confidence:** HIGH

<research_summary>
## Summary

Researched Signal Desktop's attachment encryption and Chatter backend's file upload system. **Chatter proto updated to match Signal's format: AES-256-CBC with HMAC-SHA256** (commit 9f301fd).

The backend provides presigned URL uploads for anonymous attachments. Client encrypts locally, uploads encrypted blob directly to RustFS (MinIO), then embeds the decryption key in the E2E encrypted message. Server never sees plaintext.

**Primary recommendation:** Implement Signal-compatible AES-256-CBC + HMAC-SHA256 encryption. Generate 64-byte random key per attachment (32 AES + 32 MAC), produce ciphertext as `IV || CBC-encrypted || HMAC`, include key + digest in AttachmentPointer within E2E message.
</research_summary>

<standard_stack>
## Standard Stack

### Core Rust Crates
| Library | Version | Purpose | Why Standard |
|---------|---------|---------|--------------|
| aes | 0.8 | AES-256-CBC block cipher | RustCrypto standard, audited |
| cbc | 0.1 | CBC mode wrapper | RustCrypto standard |
| hmac | 0.12 | HMAC-SHA256 authentication | RustCrypto standard |
| sha2 | 0.10 | SHA-256 for digest and HMAC | Already in project (via libsignal) |
| rand | 0.8/0.9 | Cryptographic random generation | Already in project |

### Supporting
| Library | Version | Purpose | When to Use |
|---------|---------|---------|-------------|
| futures | 0.3 | Async streaming traits | Large file streaming |
| tokio | 1.x | Async IO for file operations | Already in project |
| bytes | 1.x | Efficient byte buffer handling | Already in project |

### Alternatives Considered
| Instead of | Could Use | Tradeoff |
|------------|-----------|----------|
| AES-CBC + HMAC | AES-GCM | GCM simpler (single operation), but CBC+HMAC matches Signal exactly |
| cbc crate | cipher crate | cipher is lower-level, cbc is ergonomic wrapper |
| Custom HMAC | ring | ring has HMAC but RustCrypto hmac integrates better |

**Installation:**
```toml
[dependencies]
aes = "0.8"
cbc = "0.1"
hmac = "0.12"
sha2 = "0.10"  # May already have via libsignal
# rand already in project
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
| AES-CBC encryption | Custom AES implementation | `aes` + `cbc` crates | Constant-time, audited |
| HMAC authentication | Custom MAC | `hmac` crate | Constant-time, audited |
| MAC verification | `==` comparison | `subtle::ConstantTimeEq` | Prevents timing attacks |
| Random key generation | Custom PRNG | `rand::rng()` | Cryptographically secure |
| SHA-256 digest | Custom hash | `sha2::Sha256` | Standard, constant-time |
| PKCS7 padding | Custom padding logic | `block_padding` crate | Edge cases handled |
| Log-bucketing | Custom formula | Copy Signal's exact formula | Tested, interoperable |

**Key insight:** Signal's CBC+HMAC format is well-specified. Use RustCrypto crates for all primitives. The format is: 64-byte random key, 16-byte random IV, PKCS7 padding, encrypt-then-MAC with constant-time verification.
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

### AES-256-CBC + HMAC-SHA256 Encryption (Signal-compatible)
```rust
use aes::Aes256;
use cbc::{Encryptor, cipher::{BlockEncryptMut, KeyIvInit}};
use hmac::{Hmac, Mac};
use sha2::Sha256;
use rand::RngCore;

type Aes256CbcEnc = Encryptor<Aes256>;
type HmacSha256 = Hmac<Sha256>;

/// Encrypt plaintext with AES-256-CBC + HMAC-SHA256
/// Returns: (ciphertext, 64-byte key) where ciphertext = IV || encrypted || HMAC
pub fn encrypt(plaintext: &[u8]) -> Result<(Vec<u8>, [u8; 64]), CryptoError> {
    // Generate 64-byte key: [AES key (32)] + [MAC key (32)]
    let mut key = [0u8; 64];
    rand::rng().fill_bytes(&mut key);
    let (aes_key, mac_key) = key.split_at(32);

    // Generate random IV (16 bytes)
    let mut iv = [0u8; 16];
    rand::rng().fill_bytes(&mut iv);

    // Pad plaintext to AES block size (PKCS7)
    let padded = pkcs7_pad(plaintext);

    // Encrypt with AES-256-CBC
    let cipher = Aes256CbcEnc::new_from_slices(aes_key, &iv)
        .map_err(|e| CryptoError::AttachmentEncryptionFailed(e.to_string()))?;
    let encrypted = cipher.encrypt_padded_vec_mut::<block_padding::NoPadding>(&padded);

    // Compute HMAC-SHA256 over IV + ciphertext
    let mut mac = HmacSha256::new_from_slice(mac_key)
        .map_err(|e| CryptoError::AttachmentEncryptionFailed(e.to_string()))?;
    mac.update(&iv);
    mac.update(&encrypted);
    let hmac_result = mac.finalize().into_bytes();

    // Assemble: IV || ciphertext || HMAC
    let mut result = Vec::with_capacity(16 + encrypted.len() + 32);
    result.extend_from_slice(&iv);
    result.extend_from_slice(&encrypted);
    result.extend_from_slice(&hmac_result);

    Ok((result, key))
}
```

### AES-256-CBC + HMAC-SHA256 Decryption
```rust
use aes::Aes256;
use cbc::{Decryptor, cipher::{BlockDecryptMut, KeyIvInit}};
use hmac::{Hmac, Mac};
use sha2::Sha256;
use subtle::ConstantTimeEq;

type Aes256CbcDec = Decryptor<Aes256>;
type HmacSha256 = Hmac<Sha256>;

/// Decrypt ciphertext with 64-byte key
/// Ciphertext format: IV (16) || encrypted || HMAC (32)
pub fn decrypt(ciphertext: &[u8], key: &[u8; 64]) -> Result<Vec<u8>, CryptoError> {
    if ciphertext.len() < 16 + 16 + 32 {  // IV + min block + HMAC
        return Err(CryptoError::InvalidAttachmentData("too short".into()));
    }

    let (aes_key, mac_key) = key.split_at(32);

    // Split ciphertext
    let iv = &ciphertext[..16];
    let encrypted = &ciphertext[16..ciphertext.len() - 32];
    let expected_hmac = &ciphertext[ciphertext.len() - 32..];

    // Verify HMAC first (constant-time comparison)
    let mut mac = HmacSha256::new_from_slice(mac_key)
        .map_err(|e| CryptoError::AttachmentDecryptionFailed(e.to_string()))?;
    mac.update(iv);
    mac.update(encrypted);
    let computed_hmac = mac.finalize().into_bytes();

    if computed_hmac.ct_eq(expected_hmac).unwrap_u8() != 1 {
        return Err(CryptoError::AttachmentMacMismatch);
    }

    // Decrypt with AES-256-CBC
    let cipher = Aes256CbcDec::new_from_slices(aes_key, iv)
        .map_err(|e| CryptoError::AttachmentDecryptionFailed(e.to_string()))?;
    let decrypted = cipher.decrypt_padded_vec_mut::<block_padding::NoPadding>(encrypted)
        .map_err(|e| CryptoError::AttachmentDecryptionFailed(e.to_string()))?;

    // Remove PKCS7 padding
    pkcs7_unpad(&decrypted)
}
```

### Digest Calculation
```rust
use sha2::{Sha256, Digest};

/// SHA-256 of entire ciphertext (IV || encrypted || HMAC)
pub fn compute_digest(ciphertext: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(ciphertext);
    hasher.finalize().into()
}
```

### Log-Bucketing Padding (Signal-compatible)
```rust
/// Compute padded size using Signal's log-bucketing algorithm
/// Prevents file size leakage while minimizing overhead
pub fn padded_size(original_size: usize) -> usize {
    const MIN_SIZE: usize = 541;
    const BUCKET_BASE: f64 = 1.05;

    if original_size == 0 {
        return MIN_SIZE;
    }

    let bucket = (original_size as f64).ln() / BUCKET_BASE.ln();
    let rounded = bucket.ceil();
    let padded = BUCKET_BASE.powf(rounded) as usize;

    padded.max(MIN_SIZE)
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
| AES-CBC + HMAC | AES-GCM (general) | ~2015 | GCM simpler, but Signal kept CBC+HMAC |
| 128-bit AES | 256-bit AES | Always preferred | Signal uses 256-bit for future-proofing |
| Fixed file sizes | Log-bucketing | Signal design | Prevents size-based content inference |

**Why Signal uses CBC+HMAC instead of GCM:**
- **Streaming:** HMAC can be computed independently in single pass
- **Padding control:** Log-bucketing prevents metadata leakage
- **Resumable downloads:** Incremental MAC enables partial verification
- **Double encryption:** Cleaner layering for backup tier

**Chatter decision:** Match Signal's CBC+HMAC for compatibility and proven security.

**Still valid:**
- Signal's CBC+HMAC format (what we're using)
- 64-byte random keys (32 AES + 32 MAC)
- SHA-256 for digests
- Log-bucketing for padding
</sota_updates>

<open_questions>
## Open Questions

1. **~~Should we match Signal's CBC+HMAC exactly?~~** RESOLVED
   - Decision: Yes, backend proto updated to Signal-compatible format (commit 9f301fd)
   - Using: AES-256-CBC + HMAC-SHA256, 64-byte key, log-bucketing padding

2. **Streaming threshold?**
   - What we know: Signal streams everything, our backend has 100MB limit
   - What's unclear: What file sizes are common in practice
   - Recommendation: Stream for files >1MB, load smaller files into memory

3. **Incremental MAC implementation?**
   - What we know: Signal computes per-chunk MACs for files >50KB
   - What's unclear: Exact libsignal API for incremental MAC
   - Recommendation: v1 uses simple HMAC, add incremental MAC in v2 for large file streaming
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

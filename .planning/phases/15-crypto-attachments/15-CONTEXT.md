# Phase 15: Crypto Attachments - Context

**Gathered:** 2026-02-03
**Status:** Ready for planning

<vision>
## How This Should Work

Signal-compatible attachment encryption using AES-256-CBC + HMAC-SHA256. When a user attaches a file to a message, the client:
1. Generates a random 64-byte key (32 AES + 32 MAC)
2. Pads plaintext using log-bucketing (prevents size inference)
3. Encrypts: IV (16 bytes) || AES-CBC(padded) || HMAC-SHA256 (32 bytes)
4. Computes digest = SHA256(ciphertext)
5. Uploads encrypted blob to CDN (gets cdn_key)
6. Includes encryption_key + digest + cdn_key in AttachmentPointer
7. Embeds AttachmentPointer in E2E encrypted message

On the receiving end, the client retrieves the encrypted blob from cdn_key, verifies digest, verifies HMAC, decrypts with AES key, removes padding. The server never sees the plaintext file content.

Backend proto updated in commit 9f301fd to match this format.

</vision>

<essential>
## What Must Be Nailed

- **Correct crypto** — AES-256-CBC + HMAC-SHA256 with proper IV, constant-time MAC verification
- **Memory efficiency** — Handle large files without loading everything into RAM (streaming)
- **Signal compatibility** — Match Signal's exact format (64-byte key, log-bucketing, encrypt-then-MAC)

All three are equally important for this phase.

</essential>

<boundaries>
## What's Out of Scope

- File upload/download (network transfer) — that's Phase 19 (Network Supporting Clients)
- Thumbnail generation — that's Phase 52 (Media File Handling)
- Progress callbacks for long operations — can come later
- File type detection/MIME handling — that's media layer concern

This phase is strictly the encrypt/decrypt operations themselves.

</boundaries>

<specifics>
## Specific Ideas

- Follow Signal's attachment encryption format (AES-256-GCM with random key)
- Key material returned from encrypt operation for inclusion in message
- Streaming API for large files (encrypt/decrypt without loading entire file into memory)

</specifics>

<notes>
## Additional Context

This is the final crypto layer phase. After this, the crypto module will have:
- Signal Protocol stores (Phase 10)
- Identity & sessions (Phase 11)
- Prekeys (Phase 12)
- Sender keys for groups (Phase 13)
- Sealed sender for anonymity (Phase 14)
- Attachment encryption (this phase)

Network layer begins at Phase 16.

</notes>

---

*Phase: 15-crypto-attachments*
*Context gathered: 2026-02-03*

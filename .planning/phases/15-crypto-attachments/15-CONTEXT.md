# Phase 15: Crypto Attachments - Context

**Gathered:** 2026-02-03
**Status:** Ready for planning

<vision>
## How This Should Work

Signal-style attachment encryption. When a user attaches a file to a message, the client:
1. Generates a random symmetric key
2. Encrypts the file with AES-GCM
3. Sends the encrypted blob separately (to storage)
4. Includes the decryption key in the E2E encrypted message

On the receiving end, the client retrieves the encrypted blob, uses the key from the message to decrypt it. The server never sees the plaintext file content.

</vision>

<essential>
## What Must Be Nailed

- **Correct crypto** — Standard AES-GCM with proper IV/nonce handling, no vulnerabilities
- **Memory efficiency** — Handle large files without loading everything into RAM
- **Signal compatibility** — Match Signal's format for potential interoperability

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

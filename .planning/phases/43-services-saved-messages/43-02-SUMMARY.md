# Phase 43-02: Client-Side Encryption for Saved Messages

## What was implemented

Added client-side encryption for saved messages using XChaCha20-Poly1305 AEAD encryption with the user's profile key.

### SavedMessageCrypto (src/crypto/saved_message_crypto.rs)

- **encrypt()**: Encrypt plaintext bytes with XChaCha20-Poly1305
  - Generates random 24-byte nonce
  - Returns EncryptedData proto with version, nonce, ciphertext, auth_tag
- **decrypt()**: Decrypt EncryptedData back to plaintext
  - Validates version (SimpleE2e)
  - Validates nonce length (24 bytes)
  - Reconstructs ciphertext with tag for decryption
- **encrypt_text()**: Encrypt text content for saving
  - Wraps in SavedMessageContent::Encrypted
- **decrypt_content()**: Decrypt SavedMessageContent to text
- **encrypt_source()**: Encrypt source metadata (channel_id, message_id, author_id, timestamp)
  - Serializes to JSON before encryption
- **decrypt_source()**: Decrypt source metadata back to SavedMessageSourcePlain

### SettingsRepository (src/storage/repositories/settings_repo.rs)

- Added `PROFILE_KEY` constant to keys module for storing the 32-byte encryption key (base64 encoded)

### SavedMessageService (src/services/saved_message_service.rs)

- Added `Encryption` error variant wrapping CryptoError
- Added `get_profile_key()`: Retrieves and validates the profile key from settings
- Added encryption helper methods:
  - `encrypt_text()`: Encrypt text for saving
  - `encrypt_source()`: Encrypt source metadata
  - `decrypt_content()`: Decrypt message content
  - `decrypt_source()`: Decrypt source metadata
- Added `proto_to_domain_saved_message_decrypted()`: Converts proto with decryption

## Test coverage

11 tests in saved_message_crypto:
- Encrypt/decrypt roundtrip
- Text encrypt/decrypt roundtrip
- Source encrypt/decrypt roundtrip
- Different plaintexts produce different ciphertexts
- Invalid key fails decryption
- Invalid auth tag fails decryption
- Empty plaintext works
- Invalid version fails
- Invalid nonce length fails
- Empty content fails
- Large plaintext works

4 new tests in saved_message_service:
- encrypt_text/decrypt_content roundtrip
- encrypt_source/decrypt_source roundtrip
- Error when profile key not set
- Error when profile key invalid length

## Security notes

- XChaCha20-Poly1305 provides authenticated encryption (AEAD)
- 24-byte nonce prevents nonce reuse with random generation
- Profile key is 32 bytes (256-bit security)
- Server never sees plaintext - true E2E encryption
- Uses CryptoVersion::SimpleE2e (version 1) per proto definition

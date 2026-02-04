---
phase: 27-mailbox-dm-system
plan: 01
status: complete
---

# Phase 27-01 Summary: DM Conversation State Management

## What Was Built

Implemented DM conversation state and mailbox lifecycle management for two-way anonymity in direct messages.

### Components Created

1. **Storage Migration v012** (`src/storage/migrations/v012_dm_conversations.rs`)
   - `dm_mailboxes`: Active DM mailboxes for receiving (Ed25519 keys, signatures, expiry)
   - `dm_peer_mailboxes`: Peer mailbox addresses for sending
   - `dm_conversation_keys`: Shared conversation keys (X25519 ECDH derived)
   - Indexes on `conversation_id` for efficient lookups

2. **DmConversation Struct** (`src/mailbox/dm/conversation.rs`)
   - `DmConversation`: Per-peer mailbox state container
   - `DmMailbox`: Our ephemeral mailbox (32-byte Ed25519 public, 64-byte private, signature)
   - `PeerMailbox`: Peer's mailbox address for sending
   - `ConversationKey`: X25519 keypair with shared secret derivation

3. **State Methods**
   - `is_ready_to_send()`: Requires peer mailbox + established shared secret
   - `is_ready_to_receive()`: Requires active (non-expired) mailbox
   - `needs_bootstrap()`: True if no mailbox exists
   - `mailbox_needs_refresh()`: True if expires within 24 hours
   - `sign_challenge()`: Ed25519 signing for polling authentication
   - `derive_shared_secret()`: X25519 ECDH for encryption

4. **DmConversationManager** (`src/mailbox/dm/conversation.rs`)
   - `load_conversation()`: Full state retrieval from database
   - `save_our_mailbox()`: Store ephemeral mailbox
   - `save_peer_mailbox()`: Store peer's address
   - `save_conversation_key()`: Store X25519 keys
   - `update_poll_time()`: Track last poll timestamp
   - `get_active_mailbox_keys()`: Retrieve all active mailboxes for polling
   - `get_expiring_mailboxes()`: Find mailboxes needing refresh
   - `cleanup_expired()`: Delete expired mailbox state
   - `delete_conversation()`: Remove all state for a peer

## Technical Decisions

1. **Separate Ed25519 and X25519**: Ed25519 for mailbox identity/authentication, X25519 for message encryption key derivation (follows Signal pattern)

2. **64-byte Ed25519 private key storage**: Stores full keypair bytes (32-byte secret + 32-byte public) for compatibility with ed25519-dalek's `SigningKey::to_keypair_bytes()`

3. **While loop for list queries**: Avoids Rust lifetime issues with rusqlite's iterator pattern (as noted in plan)

4. **RFC 3339 timestamp strings**: Consistent with Phase 26 pattern for datetime storage

5. **UNIQUE constraints**: `(conversation_id, key_version)` ensures proper key rotation tracking

## Verification

- `cargo check`: Pass
- `cargo test mailbox::dm`: 10 tests pass (all conversation state tests)
- `cargo clippy`: Warnings only (type complexity, too many arguments) - no errors

## Files Changed

- `src/storage/migrations/v012_dm_conversations.rs` - New migration
- `src/storage/migrations/mod.rs` - Registered v012
- `src/mailbox/dm/conversation.rs` - Full implementation
- `src/mailbox/dm/mod.rs` - Re-exports

## Commits

- `e2fbcdd` - feat(27-01): create DM conversation storage migration v012
- `835830b` - feat(27-01): implement DmConversation and state structs

## Next Phase

Phase 27-01 complete. Ready for Phase 27-02: DM Bootstrap (initial sealed sender to mailbox exchange).

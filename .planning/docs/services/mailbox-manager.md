# MailboxManager

**File**: `src/services/mailbox_manager.rs`

Privacy-preserving DM mailbox operations.

## Purpose

MailboxManager handles DM privacy through anonymous mailboxes:
- Creating mailboxes for receiving DMs
- Sending messages to peer's mailbox
- Managing mailbox tokens and state

## Overview

The mailbox system provides **sender anonymity**:
- Server cannot link sender to recipient
- Each conversation has unique mailbox addresses
- Messages routed through anonymous mailboxes

```
┌─────────────────────────────────────────────────────────────────┐
│ Without Mailboxes (server sees everything):                     │
│                                                                 │
│   Alice ─────────────────────────────────────────────→ Bob      │
│         Server: "Alice sent to Bob"                             │
└─────────────────────────────────────────────────────────────────┘

┌─────────────────────────────────────────────────────────────────┐
│ With Mailboxes (server sees only random addresses):             │
│                                                                 │
│   Alice → [random_mailbox_abc123] → Bob                         │
│         Server: "someone sent to mailbox abc123"                │
└─────────────────────────────────────────────────────────────────┘
```

## Methods

### Send to Mailbox

```rust
/// Send encrypted message to peer's mailbox
pub async fn send_to_mailbox(
    &self,
    peer_id: &UserId,
    encrypted_content: &[u8],
) -> Result<(), MailboxError>
```

What it does:
1. Looks up peer's receive mailbox address
2. Gets or creates blind signature token
3. Sends to mailbox via MailboxClient
4. Server cannot correlate sender identity

### Create Receive Mailbox

```rust
/// Create a new receive mailbox for incoming DMs
pub async fn create_receive_mailbox(
    &self,
    conversation_id: &str,
) -> Result<ActiveMailbox, MailboxError>
```

What it does:
1. Generates Ed25519 keypair for mailbox
2. Gets blind signature from server
3. Creates mailbox on server (anonymous)
4. Returns mailbox info for storage

### Poll Mailbox

```rust
/// Poll mailbox for new messages
pub async fn poll_mailbox(
    &self,
    mailbox_id: &str,
) -> Result<Vec<MailboxMessage>, MailboxError>
```

## ActiveMailbox Structure

```rust
pub struct ActiveMailbox {
    pub mailbox_id: String,           // Random identifier
    pub public_key: [u8; 32],         // Ed25519 public key
    pub private_key: [u8; 32],        // Ed25519 private key (local only)
    pub blind_signature_token: Vec<u8>, // Server-signed token
    pub expires_at: DateTime<Utc>,
}
```

## Usage Examples

### Send DM (via MessageService)

```rust
// MessageService internally uses MailboxManager
pub async fn send_dm_text(...) -> Result<(), ServiceError> {
    // ... encrypt with Signal ...
    // ... encrypt with mailbox shared secret ...

    // Send to peer's anonymous mailbox
    mailbox_manager.send_to_mailbox(&peer_id, &encrypted_content).await?;
}
```

### DM Bootstrap

```rust
// In dm/bootstrap.rs
// When establishing new DM conversation:

// 1. Create our receive mailbox
let our_mailbox = mailbox_manager.create_receive_mailbox(&conversation_id).await?;

// 2. Exchange mailbox addresses via key exchange
// (peer learns our mailbox, we learn theirs)
```

## DM Privacy Flow

```
Alice wants to message Bob (first time):

1. DM Bootstrap:
   Alice                                 Bob
     │                                    │
     ├─→ Create receive mailbox ←─────────┤
     │   (random address)                 │
     │                                    │
     ├─→ Exchange addresses via ──────────┤
     │   KeyExchange message              │
     │                                    │
     └─→ Store peer's mailbox ←───────────┘
         address locally

2. Subsequent Messages:
   Alice                                 Bob
     │                                    │
     ├─→ Encrypt with Signal             │
     │   (sealed sender)                  │
     │                                    │
     ├─→ Encrypt with mailbox            │
     │   shared secret                    │
     │                                    │
     └─→ Send to Bob's random ───────────→│
         mailbox address                  │
                                          │
         Server sees: "message to         │
         mailbox xyz, no sender info"     │
```

## Error Types

```rust
pub enum MailboxError {
    /// Mailbox not found
    NotFound(String),

    /// Token expired
    TokenExpired,

    /// Network error
    Network(String),

    /// Crypto error
    Crypto(String),
}
```

## Blind Signatures

MailboxManager uses blind signatures (Chaum 1982) for token creation:

1. Client generates random token
2. Client "blinds" the token (hides content from server)
3. Server signs the blinded token
4. Client "unblinds" to get valid signature
5. Client can now create mailbox anonymously

Server cannot link token request to mailbox creation.

## Dependencies

- `GrpcClientPool` - Connection to server
- `SettingsRepository` - Token storage
- `DmConversation` - Conversation state

## Related Files

- `src/mailbox/dm/bootstrap.rs` - DM setup flow
- `src/mailbox/dm/conversation.rs` - Conversation state
- `src/mailbox/dm/two_way.rs` - Two-way anonymity polling
- `src/inbox/provider.rs` - Mailbox polling loop

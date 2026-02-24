# Group Encryption Flow (Signal Protocol + NDSS '21)

Based on Signal Protocol documentation and NDSS '21 paper "Improving Signal's Sealed Sender".

## Overview

Two independent systems working together:
- **Sender Keys**: Efficient O(1) encryption for group messages
- **Ephemeral Mailboxes**: Traffic analysis resistance (server can't correlate group membership)

## Phase 1: Channel Creation (Eager Setup)

When a user creates a new group channel:

1. **Create channel on server** (gRPC) - get channel_id
2. **Generate ephemeral key pair** for this group specifically
3. **Acquire anonymous credential** - blind signature on ephemeral public key
4. **Register ephemeral mailbox** - anonymously present unblinded token to server
5. **Store mailbox locally** - DO NOT generate Sender Key yet

## Phase 2: Mailbox Announcement (Immediate)

Mailbox announcement happens **immediately** after creation/join (NOT lazy):

1. **For each initial member**:
   - Check if Double Ratchet session exists
   - If no session: perform X3DH key agreement first
   - Encrypt mailbox announcement with pairwise Double Ratchet
   - Wrap in Sealed Sender envelope
   - Send to member's **long-term ID** (device inbox)

2. **Announcement payload** (application-level proto):
   ```protobuf
   message GroupMailboxAnnouncement {
     string channel_id = 1;
     bytes ephemeral_mailbox_pk = 2;  // 32-byte Curve25519
     uint64 timestamp = 3;
   }
   ```

**Why immediate?** Members need our mailbox address to send us messages anonymously. If we delay, they'd send to our long-term ID, leaking our identity to server.

**Why pairwise encryption?** Sending plaintext would let server link mailbox to identity, defeating the purpose.

## Phase 3: Member Reciprocation

When a member receives our announcement:

1. Decrypt and store our mailbox address
2. Update routing table: future messages to us go to our ephemeral mailbox
3. Generate their own ephemeral mailbox
4. Send their announcement back (now to our ephemeral mailbox!)

After this exchange: **two-way anonymity** achieved.

## Phase 4: First Message (Lazy SKDM)

Sender Key Distribution Message (SKDM) is generated **lazily on first message**:

1. **Generate Sender Key** (Chain Key + Signature Key)
2. **Create SKDM** containing:
   - Chain Key (32 bytes)
   - Signature Public Key
3. **For each member**: encrypt SKDM pairwise (reuse existing sessions)
4. **Encrypt message content** once with Sender Key
5. **Fan-out**: send pairwise SKDMs + single group ciphertext to all member mailboxes

## Phase 5: Subsequent Messages

1. Derive Message Key from Chain Key
2. Encrypt content with AES-256-CBC
3. Sign ciphertext with Signature Private Key
4. Ratchet Chain Key forward (forward secrecy)
5. Fan-out to all member ephemeral mailboxes

Server sees: anonymous sender → N random uncorrelated mailboxes

## Phase 6: New Member Joins

1. Admin sends pairwise message to new member containing:
   - Group metadata
   - Admin's Sender Key
   - Admin's ephemeral mailbox address
2. New member does Phase 1 setup (mailbox registration)
3. New member broadcasts their SKDM + mailbox to all existing members
4. Existing members send their SKDMs to new member

**Note**: New member cannot read past messages (no historical Chain Keys).

## Implementation Notes

### What's Send-safe (can use tokio::spawn)
- `GroupCoordinator` - mailbox registration
- `ChannelClient` - gRPC channel operations
- `MailboxClient` - mailbox token operations

### What's NOT Send-safe (needs spawn_blocking)
- `SignalService` - holds non-Send SenderKeyStore
- Anything using `encrypt_message`, `encrypt_for_group`

### Implementation Status

1. **`distribute_sender_key`** (FIXED):
   - Now sends SKDM pairwise to each member via Sealed Sender
   - Uses `SignalService.encrypt_message()` for Double Ratchet encryption
   - Bundles our mailbox address via `Content.ephemeral_reply_to` field
   - Sends to member's device inbox (destination_device_id = 0 for all devices)
   - See `message_service.rs:1318`

2. **`send_control_content`** (IMPROVED):
   - Uses Sender Keys (auto-creates SKDM if needed, distributes pairwise)
   - Now accepts optional `GroupFanout` parameter for NDSS '21 mailbox delivery
   - When `group_fanout` provided: sends via both channel AND member mailboxes
   - When `group_fanout` is None: sends via channel only (fallback)
   - See `message_service.rs:294`

3. **Mailbox announcement bundling** (FIXED):
   - `Content.ephemeral_reply_to` field contains our 32-byte mailbox public key
   - Recipients extract this from SKDM messages to learn our mailbox address

### Correct Implementation Strategy

**Channel Creation Callback** (tokio::spawn - Send-safe):
1. Create channel via gRPC
2. Register ephemeral mailbox via GroupCoordinator
3. Queue mailbox announcement for async sending
4. Save channel locally

**Background Task** (spawn_blocking - has SignalService):
1. Pick up pending mailbox announcements
2. For each member: pairwise encrypt via SignalService
3. Send via Sealed Sender to device inbox

**Message Send** (spawn_blocking - has SignalService):
1. Check if we have Sender Key for group
2. If not: generate + distribute SKDM pairwise
3. Encrypt content with Sender Key
4. Fan-out to all member mailboxes

# Chatter Client Architecture

Complete architecture reference for the Rust desktop client.

## Table of Contents

1. [Boot Sequence](#1-boot-sequence)
2. [Three-Tier Architecture](#2-three-tier-architecture)
3. [Login / Register Flow](#3-login--register-flow)
4. [Message Send Flow — DM](#4-message-send-flow--dm)
5. [Message Send Flow — Channel](#5-message-send-flow--channel)
6. [Sealed Sender](#6-sealed-sender-anonymous-delivery)
7. [Message Receive Flow](#7-message-receive-flow)
8. [Event System Architecture](#8-event-system-architecture)
9. [Signal Protocol Key Hierarchy](#9-signal-protocol-key-hierarchy)
10. [Anonymous Messaging (NDSS '21)](#10-anonymous-messaging-ndss-21)
11. [Key File Map](#11-key-file-map)

---

## 1. Boot Sequence

```
main() -> app_main()
  |
  +- 1. Init tracing + anti-debug
  +- 2. Init GStreamer (media) + OpenGL (rendering)
  +- 3. ChatterClient::builder().server_url(url).build()
  |     +-- AppContext::with_config()
  |         +-- Tier 1: InfrastructureContainer (DB, crypto stores, gRPC pool)
  |         +-- Tier 2: ServiceContainer (16 services using infra)
  |         +-- Tier 3: CoreServices (services + 11 state stores + EventBus)
  +- 4. auth().restore_session() -- load saved credentials
  +- 5. auth().connect() -- gRPC to server (offline mode if fails)
  +- 6. MainWindow::new() -- Slint UI
  +- 7. Route view:
  |     +-- Logged in -> "main" view, client.start(), spawn event handlers
  |     +-- Not logged in -> "login" or "register" view
  +- 8. wire_all() -- connect all UI callbacks
  +- 9. window.run() -- Slint event loop (blocks)
```

**Entry point:** `chatter-ui/src/main.rs::app_main()`

---

## 2. Three-Tier Architecture

```
+-------------------------------------------------------+
|  Tier 3: CoreServices (UI-facing API)                  |
|  - All services + state stores + EventBus              |
|  - Post-boot OnceLocks (CallService, InboxProvider)    |
|  - ChatterClient wraps this                            |
+-------------------------------------------------------+
        |
+-------------------------------------------------------+
|  Tier 2: ServiceContainer (business logic)             |
|  - AuthService, MessageService, ChannelService         |
|  - AttachmentService, SignalService, AnonDmService     |
|  - ReactionService, TypingService, PinService, etc.    |
+-------------------------------------------------------+
        |
+-------------------------------------------------------+
|  Tier 1: InfrastructureContainer (low-level)           |
|  - SQLite database + repositories                      |
|  - SignalProtocolStore (identity, prekeys, sessions)    |
|  - gRPC client pool + token manager                    |
+-------------------------------------------------------+
```

| Tier | Contains | Init |
|------|----------|------|
| **InfrastructureContainer** | SQLite DB, SignalProtocolStore (identity/prekey/session/sender key), gRPC pool, token manager | `AppContext::new()` step 1 |
| **ServiceContainer** | 16+ domain services that use infrastructure | `AppContext::new()` step 2 |
| **CoreServices** | All services + 11 state stores (ChatState, MessageState, AuthState, etc.) + EventBus + post-boot OnceLocks | `AppContext::new()` step 3 |

**Key files:**
- `chatter-core/src/app/context.rs` — builds all three tiers
- `chatter-core/src/app/core_services.rs` — CoreServices struct and public API
- `chatter-core/src/app/mod.rs` — tier definitions
- `chatter-core/src/client.rs` — ChatterClient facade wrapping CoreServices

---

## 3. Login / Register Flow

```
User clicks Login
  |
wire_login() callback fires
  |
ui_ctx.login(&username, &password, "linux-client").await
  +- Authenticates with server (gRPC)
  +- Stores session token
  +- Generates Signal identity key pair (if first login on device)
  +- Uploads prekeys (100 one-time + 1 signed + 1 Kyber PQ)
  +- Initializes DeviceInboxProvider
  +- Starts EventHandlers background task
  +-- Returns user_id + device_id
  |
Post-login setup:
  +- Spawn DomainEvent subscription -> domain_handler
  +- Spawn anon group event handler
  +- Spawn anon DM request handler
  +- load_initial_data() -- channels, DMs, messages, friend requests, anon convos
  +- initialize_presence() -- online status
```

Register does the same but also: uploads avatar, creates identity, generates PIN for SVR (Secure Value Recovery).

**Key files:**
- `chatter-ui/src/bin_ui/features/auth/login.rs` — login UI wiring
- `chatter-ui/src/bin_ui/features/auth/register.rs` — register UI wiring
- `chatter-core/src/client.rs` — `login()` / `register()` / `start()`
- `chatter-ui/src/bin_ui/features/auth/initial_data.rs` — post-login data loading

---

## 4. Message Send Flow — DM

DMs use **two-layer encryption**: Signal Protocol (forward secrecy) + Mailbox encryption (sender anonymity).

```
User clicks Send in a DM
  |
composer callback -> ConversationHandle::send_message()
  |
OutgoingIntent::ChannelDm dispatched via route_outgoing()
  |
send_dm_content(peer_id, content_proto)
  |
  +- LAYER 1: Signal Protocol (per-device pairwise encryption)
  |   signal_service.encrypt_message_all_devices(peer_id, plaintext)
  |     +- Load Double Ratchet session for each peer device
  |     +- If no session -> X3DH key exchange:
  |     |   +- Fetch peer's PreKeyBundle from server
  |     |   +- X3DH: IK_A, EK_A x IK_B, SPK_B, OPK_B -> shared_secret
  |     |   +- Initialize Double Ratchet with shared_secret
  |     |   +-- First message is a PreKeyWhisperMessage
  |     +- Ratchet forward -> derive message key
  |     +-- AES-256-CBC encrypt with message key -> WhisperMessage
  |
  +- LAYER 2: Mailbox Encryption (sender anonymity)
  |   encrypt_dm_bytes(signal_ciphertext, conversation_shared_secret)
  |     +- Random 24-byte nonce
  |     +-- XChaCha20-Poly1305(signal_ciphertext, nonce, shared_secret)
  |         -> Nonce(24) || Ciphertext || Tag(16)
  |
  +-- mailbox_manager.send_to_mailbox(peer_mailbox_pk, encrypted_bytes)
       +-- Server stores in peer's ephemeral mailbox, fans out to all peer devices
```

**Why two layers?** Layer 1 (Signal) provides end-to-end encryption with forward secrecy. Layer 2 (Mailbox) provides sender anonymity — the server sees which mailbox received a message but can't tell who sent it, because the XChaCha20 key is only known to the two participants.

**Bootstrap:** First message triggers DM bootstrap via X3DH. `BootstrapInitiated` error returned, message queued. After handshake completes, `send_pending_messages()` drains the queue.

**Key files:**
- `chatter-core/src/services/message/dm.rs` — DM send logic
- `chatter-core/src/crypto/dm_crypto.rs` — XChaCha20-Poly1305 mailbox layer
- `chatter-core/src/crypto/signal/store.rs` — Signal Protocol session store
- `chatter-core/src/services/gateway/outgoing.rs` — OutgoingIntent dispatch (9 variants: ChannelDm, Channel, AnonDm, AnonGroup, SavedMessages, CallOffer, CallAnswer, CallHangup, Connect)

---

## 5. Message Send Flow — Channel

Channel messages use **Sender Keys** (one encryption for all members) + **fan-out** (pairwise delivery to mailboxes).

```
User clicks Send in a channel
  |
OutgoingIntent::Channel dispatched
  |
send_message(channel_id, content_proto)
  |
  +- Check: is channel encrypted?
  |
  +- If encrypted:
  |   prepare_encrypted_content()
  |     +- Load/create SenderKeyDistributionMessage (SKDM) for this channel
  |     +- group_encrypt(plaintext, sender_key_store, distribution_id)
  |     |   +-- Single encryption: one ciphertext for ALL members (O(1) not O(N))
  |     +- If new SKDM: distribute_sender_key()
  |           +-- For each member: encrypt SKDM pairwise via sealed sender
  |               (Double Ratchet -> each member gets their own encrypted copy)
  |
  +- Send to server API (server stores in channel history for scroll-back)
  |
  +-- Parallel: GroupFanout.send_to_group_pairwise()
       +- For each member:
       |   +- Sealed sender encrypt(content, sender_cert, member_address)
       |   +-- Send to member's ephemeral mailbox
       +-- All members receive simultaneously (parallel async tasks)
```

**Why both server API + fan-out?** The server API stores the sender-key-encrypted message in channel history (for scroll-back / loading older messages). The fan-out sends pairwise-encrypted copies to each member's ephemeral mailbox for real-time delivery with sealed sender anonymity.

**Key files:**
- `chatter-core/src/services/message/core.rs` — channel send logic
- `chatter-core/src/crypto/signal/sender_key.rs` — sender key encrypt/decrypt
- `chatter-core/src/mailbox/group/fanout.rs` — parallel fan-out to member mailboxes

---

## 6. Sealed Sender (Anonymous Delivery)

```
sealed_sender_encrypt(plaintext, sender_certificate, recipient)
  +- sender_certificate: signed by server, contains sender identity + expiry
  +- Encrypt plaintext with recipient's Signal session (Double Ratchet)
  +- Wrap in UnidentifiedSenderMessageContent (USMC):
  |   +- Encrypted content
  |   +- Sender certificate (encrypted, only recipient can read)
  |   +-- Content type + content hints
  +-- Server routes by recipient address but cannot read sender identity
```

**UAK (Unidentified Access Key):**
```
generate_uak_from_profile_key(profile_key: [u8; 32]) -> [u8; 16]
  cipher = AES-256-GCM(profile_key)
  plaintext = 16 zero bytes
  UAK = AES-GCM.encrypt(plaintext, nonce=zeros)[0..16]
```

The server validates the UAK to authorize delivery without knowing the sender. For groups, UAKs are XOR'd together: `combine_uaks_for_group(uaks) -> XOR all recipient UAKs`.

**Key files:**
- `chatter-core/src/crypto/signal/sealed_sender.rs` — UAK generation, cert validation
- `chatter-core/src/crypto/envelope_crypto.rs` — stateless NDSS'21 envelope encryption

---

## 7. Message Receive Flow

```
SERVER                          CLIENT
  |                               |
  |   DeviceInboxProvider.run_loop() polls:
  |     +- Device inbox (control messages, SKDM, sender key messages)
  |     +- DM mailboxes (encrypted DMs)
  |     +- Group mailboxes (encrypted channel messages)
  |     +- Anon DM/group mailboxes
  |                               |
  +---- encrypted bytes -------->-|
  |                               |
  |                     DECRYPTION LAYER
  |                     +- Device inbox messages:
  |                     |   MessageGateway.process_incoming_message()
  |                     |     +- sealed_sender_decrypt() -> sender identity
  |                     |     +- Double Ratchet decrypt -> plaintext
  |                     |     +- Parse Content protobuf
  |                     |     +-- Route by content type -> HandlerRegistry
  |                     |         +- ChatHandler -> DomainEvent::MessageReceived
  |                     |         +- TypingHandler -> DomainEvent::TypingStarted
  |                     |         +- ReactionHandler -> DomainEvent::ReactionAdded
  |                     |         +- ReceiptHandler -> DomainEvent::MessagesRead
  |                     |         +- PinHandler -> DomainEvent::MessagePinned
  |                     |         +- SenderKeyHandler -> store sender key
  |                     |         +- KeyExchangeHandler -> DM bootstrap
  |                     |
  |                     +- DM mailbox messages:
  |                     |   poll_dm_mailboxes()
  |                     |     +- XChaCha20-Poly1305 decrypt (mailbox layer)
  |                     |     +- Signal Protocol decrypt (Double Ratchet)
  |                     |     +- Store message in DB
  |                     |     +-- Emit InboxEvent::DmReceived
  |                     |
  |                     +-- Zeroize all plaintext after processing
  |                               |
  |                     EVENT PIPELINE
  |                               |
  |                     InboxEvent broadcast (capacity 256)
  |                       +-->  InboxEventProcessor
  |                       |     +- Create attachment DB records
  |                       |     +- Send delivery receipt (async)
  |                       |     +- Pre-fetch sender profile
  |                       |     +- Emit DomainEvent::MessageReceived { message }
  |                       |     +-- Return true (fully handled)
  |                       |
  |                       +-- (UI does not subscribe to InboxEvent)
  |                               |
  |                     DomainEvent broadcast (capacity 64)
  |                       +-->  EventHandlers (core)
  |                       |     +- MessageState.append_message()
  |                       |     +- Persist to DB
  |                       |     +-- Update typing/receipt/presence state
  |                       |
  |                       +-->  domain_handler (UI)
  |                             +- Resolve sender name + attachments
  |                             +- Insert into Slint message model
  |                             +- Update sidebar preview
  |                             +- Trigger attachment auto-download
  |                             +-- Clear typing indicator
  |                               |
  |                     MESSAGE APPEARS IN UI
```

**Security features in the receive path:**
- Sender anonymity: mailbox layer decryption reveals sender only to recipient
- Forward secrecy: Double Ratchet advances with each message
- Access pattern obfuscation: NDSS'21 timing randomization + dummy polls
- Plaintext zeroization: decrypted content wiped from memory after processing
- Deduplication: prevents re-decryption and "old counter" Signal ratchet errors

**Key files:**
- `chatter-core/src/inbox/provider.rs` — polling loop orchestrator
- `chatter-core/src/inbox/polling/dm.rs` — DM mailbox polling
- `chatter-core/src/services/gateway/decryption.rs` — Signal Protocol decryption
- `chatter-core/src/inbox/handlers/mod.rs` — HandlerRegistry + DomainEvent definitions
- `chatter-core/src/services/inbox_processor.rs` — protocol-level processing + DomainEvent bridge

---

## 8. Event System Architecture

```
+-------------------------------------------------------+
|              DeviceInboxProvider                        |
|  run_loop(): polls device inbox, DM mailboxes, group   |
|  mailboxes every ~5s with NDSS'21 timing obfuscation   |
+----------------------------+---------------------------+
                             | emits
                    +--------v--------+
                    |   InboxEvent    |  50 variants
                    |  (broadcast/256)|  Low-level polling events
                    +---+--------+----+
                        |        |
          +-------------v--+  +--v-------------------+
          | InboxEvent     |  |  HandlerRegistry     |
          | Processor      |  |  (content routing)   |
          |                |  |  ChatHandler          |
          | Protocol:      |  |  TypingHandler        |
          | - receipts     |  |  ReactionHandler      |
          | - attachments  |  |  ReceiptHandler       |
          | - profiles     |  |  PinHandler           |
          | - sender keys  |  |  SenderKeyHandler     |
          |                |  |  KeyExchangeHandler   |
          +-------+--------+  +-----------+-----------+
                  | emits                 | emits
                  +----------+------------+
                    +--------v--------+
                    |  DomainEvent    |  58 variants
                    |  (broadcast/64) |  Clean domain events
                    +---+--------+----+
                        |        |
          +-------------v--+  +--v-------------------+
          | EventHandlers  |  |  UI domain_handler   |
          | (core)         |  |  (Slint updates)     |
          |                |  |                      |
          | Updates:       |  | Updates:             |
          | - MessageState |  | - Message model      |
          | - TypingState  |  | - Sidebar preview    |
          | - PinnedState  |  | - Unread badges      |
          | - PresenceState|  | - Typing indicators  |
          | - DB persist   |  | - Call modals        |
          +----------------+  +----------------------+
```

**Key points:**
- InboxEvent is the low-level event from polling (50 variants)
- DomainEvent is the clean domain event for business logic (58 variants)
- InboxEventProcessor bridges InboxEvent -> DomainEvent (handles protocol, emits domain events)
- HandlerRegistry routes decrypted content by type to specific handlers that emit DomainEvents
- EventHandlers updates in-memory state stores from DomainEvents
- UI domain_handler updates Slint models from DomainEvents
- The UI does NOT subscribe to InboxEvent — only to DomainEvent

**Key files:**
- `chatter-core/src/events/bus.rs` — EventBus (domain + UI broadcast channels)
- `chatter-core/src/events/handlers.rs` — EventHandlers (state store updates)
- `chatter-core/src/inbox/handlers/mod.rs` — HandlerRegistry + handler trait
- `chatter-core/src/services/inbox_processor.rs` — InboxEvent -> DomainEvent bridge
- `chatter-ui/src/bin_ui/features/auth/inbox/domain_handler/mod.rs` — UI event dispatch

---

## 9. Signal Protocol Key Hierarchy

```
Identity Key Pair (Ed25519, permanent)
  +- Identifies this device across all conversations
  +- Used for X3DH initial key agreement
  +-- Trust On First Use (TOFU) -- archived on change

Signed PreKey (Curve25519, rotated periodically)
  +- Signed by Identity Key
  +-- Used in X3DH when no one-time prekeys available

One-Time PreKeys (Curve25519, single-use, 100 uploaded)
  +- Consumed during X3DH
  +-- Server deletes after use, client replenishes

Kyber PreKeys (ML-KEM-768, post-quantum)
  +- PQXDH extension for quantum resistance
  +-- Combined with X25519 ECDH for hybrid security

Session (Double Ratchet state, per peer device)
  +- Root key -> Chain key -> Message key
  +- DH ratchet: new ephemeral key per message exchange
  +- Symmetric ratchet: KDF chain for consecutive messages
  +-- Forward secrecy: old keys deleted after use

Sender Keys (per group, per sender)
  +- One encryption for all group members (O(1) vs O(N))
  +- Distributed via SKDM through pairwise Double Ratchet
  +-- Ratcheted forward with each message

Profile Key (32 bytes, per user)
  +-- Derives UAK for sealed sender: AES-GCM(profile_key, zeros) -> UAK
```

**Key files:**
- `chatter-core/src/crypto/signal/store.rs` — implements 6 libsignal traits (IdentityKeyStore, PreKeyStore, SignedPreKeyStore, KyberPreKeyStore, SessionStore, SenderKeyStore)
- `chatter-core/src/crypto/signal/identity.rs` — identity key management
- `chatter-core/src/crypto/signal/sender_key.rs` — sender key create/process/encrypt/decrypt

---

## 10. Anonymous Messaging (NDSS '21)

### Anonymous DMs

```
Initiator                              Responder
    |                                      |
    +- ECIES encrypt(intro_msg,            |
    |   responder_identity_pk)             |
    |   + own mailbox_pk + identity_pk     |
    +------------- init message ---------->|
    |                                      +- ECIES decrypt with identity_sk
    |                                      +- Verify initiator identity
    |                                      +- Create conversation record
    |                                      +- Generate shared_secret via ECDH
    |                                      |   (initiator_pk x responder_sk)
    |<------------ accept message ---------+
    +- Create conversation record          |
    +- Derive same shared_secret           |
    |                                      |
    |  All subsequent messages:            |
    |  XChaCha20-Poly1305(msg, shared_secret)
    |  Sent to peer's ephemeral mailbox    |
    |  Server never sees who's talking     |
```

### Anonymous Groups

```
Owner creates group
  +- Generate group_id (random 16 bytes)
  +- Create group mailbox (ephemeral keypair)
  +- For each invitee:
  |   +- Encrypt invite via pairwise sealed sender
  |   +-- Include: group_id, group_name, mailbox_pk, owner profile

Members join
  +- Decrypt invite -> get group_id + mailbox_pk
  +- Send JoinAck to group mailbox
  +-- Receive member list from owner

Messages
  +- Sender encrypts with group sender key
  +- GroupFanout distributes to all member mailboxes
  +-- Each member decrypts with stored sender key

Server sees: N messages to N mailboxes.
Cannot link members, sender, or content.
```

**Key files:**
- `chatter-core/src/crypto/ecies.rs` — ECIES encryption for init messages
- `chatter-core/src/crypto/envelope_crypto.rs` — stateless NDSS'21 mailbox envelope
- `chatter-core/src/services/anon_dm/mod.rs` — anonymous DM service
- `chatter-core/src/services/anon_group/` — anonymous group service
- `chatter-core/src/mailbox/group/fanout.rs` — group message fan-out

---

## 11. Key File Map

| Component | File |
|-----------|------|
| **App entry** | `chatter-ui/src/main.rs` |
| **Client facade** | `chatter-core/src/client.rs` |
| **3-tier init** | `chatter-core/src/app/context.rs` |
| **CoreServices API** | `chatter-core/src/app/core_services.rs` |
| **Message routing** | `chatter-core/src/services/gateway/outgoing.rs` |
| **DM encryption** | `chatter-core/src/services/message/dm.rs` |
| **Channel encryption** | `chatter-core/src/services/message/core.rs` |
| **Signal store** | `chatter-core/src/crypto/signal/store.rs` |
| **Sealed sender** | `chatter-core/src/crypto/signal/sealed_sender.rs` |
| **Sender keys** | `chatter-core/src/crypto/signal/sender_key.rs` |
| **Envelope crypto** | `chatter-core/src/crypto/envelope_crypto.rs` |
| **DM mailbox crypto** | `chatter-core/src/crypto/dm_crypto.rs` |
| **Attachment crypto** | `chatter-core/src/crypto/attachment_crypto.rs` |
| **Polling loop** | `chatter-core/src/inbox/provider.rs` |
| **Decryption gateway** | `chatter-core/src/services/gateway/decryption.rs` |
| **Handler registry** | `chatter-core/src/inbox/handlers/mod.rs` |
| **InboxEventProcessor** | `chatter-core/src/services/inbox_processor.rs` |
| **EventBus** | `chatter-core/src/events/bus.rs` |
| **EventHandlers** | `chatter-core/src/events/handlers.rs` |
| **Group fanout** | `chatter-core/src/mailbox/group/fanout.rs` |
| **UI domain handler** | `chatter-ui/src/bin_ui/features/auth/inbox/domain_handler/mod.rs` |
| **ECIES (anon init)** | `chatter-core/src/crypto/ecies.rs` |
| **Anon DM service** | `chatter-core/src/services/anon_dm/mod.rs` |
| **Conversation handle** | `chatter-core/src/services/conversation.rs` |

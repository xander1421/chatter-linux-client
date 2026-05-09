//! Integration tests for the encryption pipeline.
//!
//! Validates the cryptographic flows described in sections 3, 6, and 7:
//!
//!   - UAK derivation from profile key (section 6)
//!   - UAK group combination (section 3: Channel Message)
//!   - DM channel ID determinism (section 3: DM)
//!   - Domain ID type safety (section 6: Key Hierarchy)
//!   - Sender certificate validation (section 3: Sealed Sender)
//!
//! These tests use pure crypto functions with no server dependencies.

use chatter_core::crypto::derive_uak;
use chatter_core::crypto::signal::sealed_sender::{
    combine_uaks_for_group,
    UAK_LENGTH, PROFILE_KEY_LENGTH,
};
use chatter_core::domain::ids::{ChannelId, DeviceId, MessageId, UserId};

// ---------------------------------------------------------------------------
// UAK (Unidentified Access Key) derivation
// ---------------------------------------------------------------------------

/// Test UAK derivation from profile key.
///
/// Architecture reference (section 6):
///   Profile Key (32 bytes, per user)
///     └─ Derives UAK for sealed sender: AES-GCM(profile_key, zeros) → UAK
///
/// The UAK is derived from the recipient's profile key:
///   AES-256-GCM(profile_key, 16 zero bytes) → 16-byte UAK
/// The server validates the UAK to authorize delivery without knowing the sender.
#[test]
fn test_uak_derivation_from_profile_key() {
    let profile_key = [42u8; PROFILE_KEY_LENGTH];
    let uak = derive_uak(&profile_key);

    assert_eq!(uak.len(), UAK_LENGTH, "UAK should be {} bytes", UAK_LENGTH);

    // UAK should not be all zeros
    assert_ne!(uak, [0u8; UAK_LENGTH], "UAK should not be all zeros");

    // UAK should not be the profile key truncated
    assert_ne!(&uak[..], &profile_key[..UAK_LENGTH], "UAK should not be truncated profile key");
}

/// Test that UAK derivation is deterministic.
///
/// Same profile key → same UAK (required for sealed sender delivery).
#[test]
fn test_uak_derivation_deterministic() {
    let profile_key = [99u8; PROFILE_KEY_LENGTH];

    let uak1 = derive_uak(&profile_key);
    let uak2 = derive_uak(&profile_key);

    assert_eq!(uak1, uak2, "Same profile key must produce same UAK");
}

/// Test that different profile keys produce different UAKs.
#[test]
fn test_uak_derivation_different_keys() {
    let key1 = [1u8; PROFILE_KEY_LENGTH];
    let key2 = [2u8; PROFILE_KEY_LENGTH];

    let uak1 = derive_uak(&key1);
    let uak2 = derive_uak(&key2);

    assert_ne!(uak1, uak2, "Different profile keys should produce different UAKs");
}

/// Profile-key length is enforced by the type system.
///
/// `derive_uak` accepts `&[u8; 32]` only; passing a shorter key fails
/// at compile time. This test asserts the runtime contract that a valid
/// 32-byte key produces a UAK of the expected length.
#[test]
fn test_uak_derivation_correct_output_length() {
    let key = [0u8; PROFILE_KEY_LENGTH];
    let uak = derive_uak(&key);
    assert_eq!(uak.len(), UAK_LENGTH);
}

// ---------------------------------------------------------------------------
// UAK group combination
// ---------------------------------------------------------------------------

/// Test UAK combination for group sealed sender.
///
/// Architecture reference (section 3: Channel Message):
///   For group messages, individual UAKs are XOR-combined into a group UAK.
///   This allows the server to validate delivery for group messages.
#[test]
fn test_uak_group_combination() {
    let uak1 = [0x11u8; UAK_LENGTH];
    let uak2 = [0x22u8; UAK_LENGTH];

    let combined = combine_uaks_for_group(&[uak1, uak2]);
    assert_eq!(combined.len(), UAK_LENGTH);

    // XOR combination: 0x11 ^ 0x22 = 0x33
    assert_eq!(combined, [0x33u8; UAK_LENGTH]);
}

/// Test that single UAK combination returns the UAK unchanged.
#[test]
fn test_uak_group_combination_single() {
    let uak = [0xABu8; UAK_LENGTH];
    let combined = combine_uaks_for_group(&[uak]);
    assert_eq!(combined, uak);
}

/// Test that combining UAK with itself produces zeros (XOR identity).
#[test]
fn test_uak_group_combination_self_cancels() {
    let uak = [0xFFu8; UAK_LENGTH];
    let combined = combine_uaks_for_group(&[uak, uak]);
    assert_eq!(combined, [0x00u8; UAK_LENGTH], "UAK XOR with itself should be zero");
}

/// Test group combination with multiple members.
#[test]
fn test_uak_group_combination_multiple() {
    let uaks = [
        [0x01u8; UAK_LENGTH],
        [0x02u8; UAK_LENGTH],
        [0x04u8; UAK_LENGTH],
    ];
    let combined = combine_uaks_for_group(&uaks);
    // 0x01 ^ 0x02 ^ 0x04 = 0x07
    assert_eq!(combined, [0x07u8; UAK_LENGTH]);
}

// ---------------------------------------------------------------------------
// DM Channel ID determinism
// ---------------------------------------------------------------------------

/// Test that DM channel IDs are deterministic from participant IDs.
///
/// Architecture reference (section 3: DM):
///   DM conversations use a deterministic channel ID derived from
///   the two participant UUIDs, regardless of who initiates.
///   Must match the server's GenerateDMChannelID function.
#[test]
fn test_dm_channel_id_deterministic() {
    let alice = UserId::new();
    let bob = UserId::new();

    let channel_ab = ChannelId::from_dm_participants(&alice, &bob);
    let channel_ba = ChannelId::from_dm_participants(&bob, &alice);

    assert_eq!(channel_ab, channel_ba, "DM channel ID must be same regardless of order");
}

/// Test that different user pairs produce different channel IDs.
#[test]
fn test_dm_channel_id_different_pairs() {
    let alice = UserId::new();
    let bob = UserId::new();
    let charlie = UserId::new();

    let channel_ab = ChannelId::from_dm_participants(&alice, &bob);
    let channel_ac = ChannelId::from_dm_participants(&alice, &charlie);

    assert_ne!(channel_ab, channel_ac, "Different pairs should produce different channel IDs");
}

/// Test that DM channel ID is a valid UUID.
#[test]
fn test_dm_channel_id_is_valid_uuid() {
    let alice = UserId::new();
    let bob = UserId::new();

    let channel_id = ChannelId::from_dm_participants(&alice, &bob);
    let uuid_str = channel_id.to_string();

    // Should be parseable as a UUID
    assert!(uuid::Uuid::parse_str(&uuid_str).is_ok(), "Channel ID should be valid UUID");
}

// ---------------------------------------------------------------------------
// Domain ID type safety
// ---------------------------------------------------------------------------

/// Test that domain IDs are strongly typed (section 6: Key Hierarchy).
///
/// UserId, ChannelId, MessageId etc. are distinct types that cannot be
/// accidentally mixed. This prevents bugs like sending to wrong entity.
#[test]
fn test_domain_id_type_safety() {
    let user_id = UserId::new();
    let channel_id = ChannelId::new();
    let message_id = MessageId::new();

    // Each ID type has its own UUID
    let user_uuid = user_id.as_uuid();
    let channel_uuid = channel_id.as_uuid();
    let message_uuid = message_id.as_uuid();

    // They should all be different (random UUIDs)
    assert_ne!(user_uuid, channel_uuid);
    assert_ne!(user_uuid, message_uuid);
    assert_ne!(channel_uuid, message_uuid);

    // Roundtrip through string
    let user_str = user_id.to_string();
    let parsed: UserId = user_str.parse().expect("Should parse back to UserId");
    assert_eq!(user_id, parsed);
}

/// Test DeviceId conventions.
///
/// Architecture reference (section 6):
///   Device ID follows Signal protocol convention.
///   Primary device = 1.
#[test]
fn test_device_id_conventions() {
    assert!(DeviceId::PRIMARY.is_primary());
    assert_eq!(DeviceId::PRIMARY.as_u32(), 1);

    let secondary = DeviceId::new(2);
    assert!(!secondary.is_primary());
}

/// Test MessageId from timestamp.
///
/// Architecture reference:
///   Signal-style message identification uses (sender, timestamp) pairs.
///   MessageId::from_timestamp creates deterministic UUID from timestamp.
#[test]
fn test_message_id_from_timestamp() {
    let id1 = MessageId::from_timestamp(1234567890);
    let id2 = MessageId::from_timestamp(1234567890);
    let id3 = MessageId::from_timestamp(9999999999);

    // Same timestamp → same ID
    assert_eq!(id1, id2, "Same timestamp must produce same MessageId");

    // Different timestamp → different ID
    assert_ne!(id1, id3, "Different timestamps should produce different MessageIds");
}

// ---------------------------------------------------------------------------
// Sealed sender constants
// ---------------------------------------------------------------------------

/// Test sealed sender constants are correct.
///
/// Architecture reference (section 3: Sealed Sender):
///   UAK is 16 bytes derived from profile key.
///   Profile key is 32 bytes.
#[test]
fn test_sealed_sender_constants() {
    assert_eq!(UAK_LENGTH, 16, "UAK should be 16 bytes");
    assert_eq!(PROFILE_KEY_LENGTH, 32, "Profile key should be 32 bytes");
}

// ---------------------------------------------------------------------------
// DM conversation types
// ---------------------------------------------------------------------------

/// Test DM conversation lifecycle states.
///
/// Architecture reference (section 3: DM Two-Layer Encryption):
///   DM conversations go through: new → bootstrap → ready states.
///   - needs_bootstrap: no peer mailbox or no shared secret
///   - is_ready_to_send: has peer mailbox + conversation key with shared_secret
///   - is_ready_to_receive: has our mailbox
#[test]
fn test_dm_conversation_lifecycle() {
    use chatter_core::mailbox::dm::conversation::DmConversation;

    let peer_id = UserId::new();
    let conv = DmConversation::new(peer_id);

    // Fresh conversation needs bootstrap
    assert!(conv.needs_bootstrap(), "New conversation should need bootstrap");
    assert!(!conv.is_ready_to_send(), "New conversation should not be ready to send");
    assert!(!conv.is_ready_to_receive(), "New conversation should not be ready to receive");
}

// ---------------------------------------------------------------------------
// Anonymous DM types
// ---------------------------------------------------------------------------

/// Test anonymous DM status transitions.
///
/// Architecture reference (section 7: Anonymous DMs):
///   AnonDmStatus: Pending → Active, or Pending → Declined/Blocked
///   Only Active status allows sending messages.
#[test]
fn test_anon_dm_status_transitions() {
    use chatter_core::domain::anon_dm::AnonDmStatus;

    // Pending status
    let status = AnonDmStatus::Pending;
    assert!(status.is_pending());
    assert!(!status.can_send(), "Pending should not allow sending");

    // Active status
    let status = AnonDmStatus::Active;
    assert!(!status.is_pending());
    assert!(status.can_send(), "Active should allow sending");

    // Blocked status
    let status = AnonDmStatus::Blocked;
    assert!(!status.can_send(), "Blocked should not allow sending");

    // Declined status
    let status = AnonDmStatus::Declined;
    assert!(!status.can_send(), "Declined should not allow sending");
}

/// Test anonymous DM creation.
///
/// Architecture reference (section 7):
///   Initiator creates AnonDm with peer_id, status=Pending, initiated_by_us=true
///   Responder creates from_request with peer mailbox pk
#[test]
fn test_anon_dm_creation() {
    use chatter_core::domain::anon_dm::AnonDm;

    let peer_id = UserId::new();

    // Outgoing request (we initiate)
    let outgoing = AnonDm::new_initiated(peer_id);
    assert!(outgoing.initiated_by_us);
    assert!(outgoing.status.is_pending());
    assert!(outgoing.is_outgoing_request());
    assert!(!outgoing.is_incoming_request());

    // Incoming request (from peer with their mailbox pk)
    let anon_dm_id = outgoing.id;
    let peer_pk = [42u8; 32];
    let incoming = AnonDm::from_request(anon_dm_id, peer_id, peer_pk);
    assert!(!incoming.initiated_by_us);
    assert!(incoming.is_incoming_request());
    assert!(!incoming.is_outgoing_request());
    assert_eq!(incoming.peer_mailbox_pk, Some(peer_pk));
}

/// Test anonymous DM message creation.
#[test]
fn test_anon_dm_message_creation() {
    use chatter_core::domain::anon_dm::{AnonDm, AnonDmMessage};

    let peer_id = UserId::new();
    let conv = AnonDm::new_initiated(peer_id);

    let msg = AnonDmMessage::new_text(
        conv.id,
        UserId::new(),
        "Hello anonymously!",
        1234567890,
    );

    assert_eq!(msg.conversation_id, conv.id);
    assert_eq!(msg.as_text().as_deref(), Some("Hello anonymously!"));
    assert_eq!(msg.sent_at, 1234567890);
    assert!(!msg.is_deleted);
    assert!(!msg.is_read);
    assert!(!msg.has_attachments());
}

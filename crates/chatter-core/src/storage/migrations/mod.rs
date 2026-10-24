//! Database migrations.
//!
//! Migration infrastructure for schema evolution with version tracking.

pub(crate) mod reaction_normalization;
pub mod v001_initial;
pub mod v002_users;
pub mod v003_channels;
pub mod v004_messages;
pub mod v005_attachments;
pub mod v006_reactions;
pub mod v007_drafts;
pub mod v009_search_index;
pub mod v010_key_store;
pub mod v011_mailbox_tokens;
pub mod v012_dm_conversations;
pub mod v013_group_mailboxes;
pub mod v015_ephemeral_messages;
pub mod v016_message_sent_at;
pub mod v017_encrypted_attachment_content;
pub mod v018_rename_svr_public_key;
pub mod v019_file_storage;
pub mod v020_anon_groups;
pub mod v021_anon_group_messages;
pub mod v022_anon_group_mailbox_poll_time;
pub mod v023_pending_channel_messages;
pub mod v024_friend_profile_keys;
pub mod v025_anon_dm;
pub mod v026_blocking_rate_limits;
pub mod v027_anon_dm_peer_display_name;
pub mod v028_anon_dm_sent_at;
pub mod v029_anon_group_sent_at;
pub mod v030_anon_dm_peer_avatar;
pub mod v031_jarvis_memory;
pub mod v032_jarvis_graph;
pub mod v033_anon_dm_reactions_receipts;
pub mod v034_anon_group_member_profiles;
pub mod v035_message_delivery_status;
pub mod v036_call_room_tokens;
pub mod v037_sender_key_rotation;
pub mod v038_anon_group_owner_id;
pub mod v039_anon_group_pinned;
pub mod v040_anon_message_parity;
pub mod v041_peer_verification;
pub mod v042_anon_message_is_edited;
pub mod v044_unified_reactions;
pub mod v045_unified_messages;
pub mod v046_reply_quote_persistence;
pub mod v047_fix_search_fts;
pub mod v048_custom_sections;
pub mod v049_vector_embeddings;
pub mod v050_pending_anon_group_messages;
pub mod v051_fix_search_json_path;
pub mod v052_reply_thumbnail;
pub mod v053_call_history;
pub mod v054_channel_icon_blob;
pub mod v055_temporal_facts;
pub mod v056_mesh_identity;
pub mod v057_mesh_store;
pub mod v058_mesh_conversations;
mod v060_mesh_delivery;
mod v061_mesh_exempt_sender_sent_at;
mod v062_section_sort_mode;
mod v063_pending_identity_changes;
mod v064_shared_content;
mod v065_sesame_tables;
mod v066_messages_sent_at_index;
mod v067_deleted_messages;
mod v068_blocked_anon_peer_name;
mod v069_peer_mailbox_device;
mod v070_peer_device_mailboxes;
mod v071_peer_device_asked;
mod v072_peer_profile_keys;
mod v073_former_channels;
mod v074_message_receipts;
mod v075_message_failure_reason;
mod v076_own_device_send_targets;
mod v077_pending_dm_messages;
mod v078_send_log_and_pending_modifiers;
mod v079_message_deletion_token;
mod v080_pending_reactions_and_pins;
mod v081_dm_mailbox_encryption_key;
mod v082_verified_channel_membership;
mod v083_mesh_sender_names;
mod v084_scoped_reactions;
mod v085_reaction_receive_journal;
mod v086_channel_message_clocks;
mod v087_single_reaction;
mod v088_reaction_normalization;
mod v089_pin_added_order;
mod v090_delete_batch_retry;
mod v092_saved_pin_watermark;
mod v093_saved_sync_outbox;
mod v094_anon_membership_order;
mod v095_saved_delete_tombstones;
mod v096_saved_delete_outbox;
mod v097_dm_outbox;
mod v098_dm_outbox_sealed_at;
mod v099_dm_receive_tombstones;
mod v100_drop_message_deletion_token;
mod v101_attachment_secret_chat;
mod v102_drop_call_room_tokens;
mod v103_secret_chat_peer_device;
mod v104_reaction_send_clocks;
mod v105_drop_legacy_reaction_evidence;
mod v106_drop_anon_group_legacy_lock;
mod v107_drop_attachment_local_path;
mod v108_drop_channel_message_clocks;
mod v109_drop_mailbox_randomizer;
mod v110_attachment_secret_chat_known;
mod v111_secret_chat_request_state;
mod v112_send_log_group_id;
mod v113_mesh_account_binding;
mod v114_mesh_signal;
mod v115_mesh_signal_courier;
mod v116_mesh_private_channels;
mod v117_mesh_conversation_kinds;
mod v118_read_horizons;
mod v119_pending_anon_group_invites;
mod v120_secret_receive_artifacts;
mod v121_anon_group_departed_profiles;
mod v122_secret_receive_control_kinds;
mod v123_attachment_cleanup;
mod v124_activity_center;
mod v125_contact_names;
mod v126_activity_outage_resolution;
mod v127_shared_content_categories;
mod v128_anon_group_invite_epoch;
mod v129_pending_dm_retry;
mod v131_mesh_archive_packets;

use crate::error::StorageError;
use chrono::Utc;
use rusqlite::Connection;

pub use v001_initial::V001Initial;
pub use v002_users::V002Users;
pub use v003_channels::V003Channels;
pub use v004_messages::V004Messages;
pub use v005_attachments::V005Attachments;
pub use v006_reactions::V006Reactions;
pub use v007_drafts::V007Drafts;
pub use v009_search_index::V009SearchIndex;
pub use v010_key_store::V010KeyStore;
pub use v011_mailbox_tokens::V011MailboxTokens;
pub use v012_dm_conversations::V012DmConversations;
pub use v013_group_mailboxes::V013GroupMailboxes;
pub use v015_ephemeral_messages::V015EphemeralMessages;
pub use v016_message_sent_at::V016MessageSentAt;
pub use v017_encrypted_attachment_content::V017EncryptedAttachmentContent;
pub use v018_rename_svr_public_key::V018RenameSvrPublicKey;
pub use v019_file_storage::V019FileStorage;
pub use v020_anon_groups::V020AnonGroups;
pub use v021_anon_group_messages::V021AnonGroupMessages;
pub use v022_anon_group_mailbox_poll_time::V022AnonGroupMailboxPollTime;
pub use v023_pending_channel_messages::V023PendingChannelMessages;
pub use v024_friend_profile_keys::V024FriendProfileKeys;
pub use v025_anon_dm::V025AnonDm;
pub use v026_blocking_rate_limits::V026BlockingRateLimits;
pub use v027_anon_dm_peer_display_name::V027AnonDmPeerDisplayName;
pub use v028_anon_dm_sent_at::V028AnonDmSentAt;
pub use v029_anon_group_sent_at::V029AnonGroupSentAt;
pub use v030_anon_dm_peer_avatar::V030AnonDmPeerAvatar;
pub use v031_jarvis_memory::V031JarvisMemory;
pub use v032_jarvis_graph::V032JarvisGraph;
pub use v033_anon_dm_reactions_receipts::V033AnonDmReactionsReceipts;
pub use v034_anon_group_member_profiles::V034AnonGroupMemberProfiles;
pub use v035_message_delivery_status::V035MessageDeliveryStatus;
pub use v036_call_room_tokens::V036CallRoomTokens;
pub use v037_sender_key_rotation::V037SenderKeyRotation;
pub use v038_anon_group_owner_id::V038AnonGroupOwnerId;
pub use v039_anon_group_pinned::V039AnonGroupPinned;
pub use v040_anon_message_parity::V040AnonMessageParity;
pub use v041_peer_verification::V041PeerVerification;
pub use v042_anon_message_is_edited::V042AnonMessageIsEdited;
pub use v044_unified_reactions::V044UnifiedReactions;
pub use v045_unified_messages::V045UnifiedMessages;
pub use v046_reply_quote_persistence::V046ReplyQuotePersistence;
pub use v047_fix_search_fts::V047FixSearchFts;
pub use v048_custom_sections::V048CustomSections;
pub use v049_vector_embeddings::V049VectorEmbeddings;
pub use v050_pending_anon_group_messages::V050PendingAnonGroupMessages;
pub use v051_fix_search_json_path::V051FixSearchJsonPath;
pub use v052_reply_thumbnail::V052ReplyThumbnail;
pub use v053_call_history::V053CallHistory;
pub use v054_channel_icon_blob::V054ChannelIconBlob;
pub use v055_temporal_facts::V055TemporalFacts;
pub use v056_mesh_identity::V056MeshIdentity;
pub use v057_mesh_store::V057MeshStore;
pub use v058_mesh_conversations::V058MeshConversations;
pub use v060_mesh_delivery::V060MeshDelivery;
pub use v061_mesh_exempt_sender_sent_at::V061MeshExemptSenderSentAt;
pub use v062_section_sort_mode::V062SectionSortMode;
pub use v063_pending_identity_changes::V063PendingIdentityChanges;
pub use v064_shared_content::V064SharedContent;
pub use v065_sesame_tables::V065SesameTables;
pub use v066_messages_sent_at_index::V066MessagesSentAtIndex;
pub use v067_deleted_messages::V067DeletedMessages;
pub use v068_blocked_anon_peer_name::V068BlockedAnonPeerName;
pub use v069_peer_mailbox_device::V069PeerMailboxDevice;
pub use v070_peer_device_mailboxes::V070PeerDeviceMailboxes;
pub use v071_peer_device_asked::V071PeerDeviceAsked;
pub use v072_peer_profile_keys::V072PeerProfileKeys;
pub use v073_former_channels::V073FormerChannels;
pub use v074_message_receipts::V074MessageReceipts;
pub use v075_message_failure_reason::V075MessageFailureReason;
pub use v076_own_device_send_targets::V076OwnDeviceSendTargets;
pub use v077_pending_dm_messages::V077PendingDmMessages;
pub use v078_send_log_and_pending_modifiers::V078SendLogAndPendingModifiers;
pub use v079_message_deletion_token::V079MessageDeletionToken;
pub use v080_pending_reactions_and_pins::V080PendingReactionsAndPins;
pub use v081_dm_mailbox_encryption_key::V081DmMailboxEncryptionKey;
pub use v082_verified_channel_membership::V082VerifiedChannelMembership;
pub use v083_mesh_sender_names::V083MeshSenderNames;
pub use v084_scoped_reactions::V084ScopedReactions;
pub use v085_reaction_receive_journal::V085ReactionReceiveJournal;
pub use v086_channel_message_clocks::V086ChannelMessageClocks;
pub use v087_single_reaction::V087SingleReaction;
pub use v088_reaction_normalization::V088ReactionNormalization;
pub use v089_pin_added_order::V089PinAddedOrder;
pub use v090_delete_batch_retry::V090DeleteBatchRetry;
pub use v092_saved_pin_watermark::V092SavedPinWatermark;
pub use v093_saved_sync_outbox::V093SavedSyncOutbox;
use v094_anon_membership_order::V094AnonMembershipOrder;

/// Trait for database migrations.
///
/// Each migration has a unique version number and SQL to apply/revert changes.
pub trait Migration {
    /// Unique version number (e.g., 1, 2, 3...).
    ///
    /// Migrations are applied in order of version number.
    fn version(&self) -> u32;

    /// Human-readable description of what this migration does.
    fn description(&self) -> &'static str;

    /// SQL to apply this migration.
    ///
    /// Can contain multiple statements separated by semicolons.
    fn up(&self) -> &'static str;

    /// Apply SQL and any serialized-data updates in the runner's transaction.
    fn apply(&self, conn: &Connection) -> Result<(), StorageError> {
        conn.execute_batch(self.up())?;
        Ok(())
    }

    /// SQL to revert this migration (optional).
    ///
    /// Returns None if rollback is not supported.
    fn down(&self) -> Option<&'static str> {
        None
    }
}

/// Runs database migrations in order.
///
/// Tracks applied migrations in a `schema_version` table and only
/// applies migrations that haven't been run yet.
pub struct MigrationRunner {
    migrations: Vec<Box<dyn Migration>>,
}

impl MigrationRunner {
    /// Create a new empty migration runner.
    pub fn new() -> Self {
        Self {
            migrations: Vec::new(),
        }
    }

    /// Register a migration with the runner.
    ///
    /// Uses builder pattern for chaining.
    pub fn register(mut self, migration: impl Migration + 'static) -> Self {
        self.migrations.push(Box::new(migration));
        self
    }

    /// Run all pending migrations.
    ///
    /// Creates the schema_version table if it doesn't exist, then applies every
    /// registered migration that is not already recorded as applied.
    ///
    /// The check is set membership, NOT `version > MAX(version)`. A high-water
    /// mark silently skips any migration whose number is below it: with 8, 14
    /// and 43 free in the sequence, adding a `v043_*` would have been applied on
    /// a fresh install and never on an existing one, and the two databases would
    /// diverge with nothing reporting it. The same watermark also made the
    /// version numbers of the registered set load-bearing in a way nobody
    /// declared.
    pub fn run(&self, conn: &Connection) -> Result<(), StorageError> {
        // Create schema_version table if not exists
        conn.execute(
            "CREATE TABLE IF NOT EXISTS schema_version (
                version INTEGER PRIMARY KEY,
                description TEXT NOT NULL,
                applied_at TEXT NOT NULL
            )",
            [],
        )?;

        // Which versions are already applied. A failure here must NOT be
        // swallowed: this used to be `.unwrap_or(0)`, which turns any read error
        // -- a locked database, a wrong passphrase, a corrupt page -- into
        // "nothing has been applied" and replays every migration from v001 over
        // a populated database.
        let mut stmt = conn.prepare("SELECT version FROM schema_version")?;
        let applied: std::collections::HashSet<u32> = stmt
            .query_map([], |row| row.get::<_, u32>(0))?
            .collect::<Result<_, _>>()?;
        drop(stmt);

        // Sort migrations by version
        let mut sorted_migrations: Vec<_> = self.migrations.iter().collect();
        sorted_migrations.sort_by_key(|m| m.version());

        // Apply pending migrations
        for migration in sorted_migrations {
            let version = migration.version();
            if !applied.contains(&version) {
                // Execute migration in a transaction
                let tx = conn.unchecked_transaction()?;

                // Execute the up SQL
                migration.apply(&tx)?;

                // Record migration in schema_version
                let now = Utc::now().to_rfc3339();
                tx.execute(
                    "INSERT INTO schema_version (version, description, applied_at) VALUES (?1, ?2, ?3)",
                    rusqlite::params![version, migration.description(), now],
                )?;

                tx.commit()?;

                tracing::info!(
                    version = version,
                    description = migration.description(),
                    "Applied migration"
                );
            }
        }

        Ok(())
    }
}

impl Default for MigrationRunner {
    fn default() -> Self {
        Self::new()
    }
}

/// Create a MigrationRunner with all migrations registered.
///
/// This is the standard way to get a runner with all migrations.
pub fn default_migrations() -> MigrationRunner {
    MigrationRunner::new()
        .register(V001Initial)
        .register(V002Users)
        .register(V003Channels)
        .register(V004Messages)
        .register(V005Attachments)
        .register(V006Reactions)
        .register(V007Drafts)
        .register(V009SearchIndex)
        .register(V010KeyStore)
        .register(V011MailboxTokens)
        .register(V012DmConversations)
        .register(V013GroupMailboxes)
        .register(V015EphemeralMessages)
        .register(V016MessageSentAt)
        .register(V017EncryptedAttachmentContent)
        .register(V018RenameSvrPublicKey)
        .register(V019FileStorage)
        .register(V020AnonGroups)
        .register(V021AnonGroupMessages)
        .register(V022AnonGroupMailboxPollTime)
        .register(V023PendingChannelMessages)
        .register(V024FriendProfileKeys)
        .register(V025AnonDm)
        .register(V026BlockingRateLimits)
        .register(V027AnonDmPeerDisplayName)
        .register(V028AnonDmSentAt)
        .register(V029AnonGroupSentAt)
        .register(V030AnonDmPeerAvatar)
        .register(V031JarvisMemory)
        .register(V032JarvisGraph)
        .register(V033AnonDmReactionsReceipts)
        .register(V034AnonGroupMemberProfiles)
        .register(V035MessageDeliveryStatus)
        .register(V036CallRoomTokens)
        .register(V037SenderKeyRotation)
        .register(V038AnonGroupOwnerId)
        .register(V039AnonGroupPinned)
        .register(V040AnonMessageParity)
        .register(V041PeerVerification)
        .register(V042AnonMessageIsEdited)
        .register(V044UnifiedReactions)
        .register(V045UnifiedMessages)
        .register(V046ReplyQuotePersistence)
        .register(V047FixSearchFts)
        .register(V048CustomSections)
        .register(V049VectorEmbeddings)
        .register(V050PendingAnonGroupMessages)
        .register(V051FixSearchJsonPath)
        .register(V052ReplyThumbnail)
        .register(V053CallHistory)
        .register(V054ChannelIconBlob)
        .register(V055TemporalFacts)
        .register(V056MeshIdentity)
        .register(V057MeshStore)
        .register(V058MeshConversations)
        .register(V060MeshDelivery)
        .register(V061MeshExemptSenderSentAt)
        .register(V062SectionSortMode)
        .register(V063PendingIdentityChanges)
        .register(V064SharedContent)
        .register(V065SesameTables)
        .register(V066MessagesSentAtIndex)
        .register(V067DeletedMessages)
        .register(V068BlockedAnonPeerName)
        .register(V069PeerMailboxDevice)
        .register(V070PeerDeviceMailboxes)
        .register(V071PeerDeviceAsked)
        .register(V072PeerProfileKeys)
        .register(V073FormerChannels)
        .register(V074MessageReceipts)
        .register(V075MessageFailureReason)
        .register(V076OwnDeviceSendTargets)
        .register(V077PendingDmMessages)
        .register(V078SendLogAndPendingModifiers)
        .register(V079MessageDeletionToken)
        .register(V080PendingReactionsAndPins)
        .register(V081DmMailboxEncryptionKey)
        .register(V082VerifiedChannelMembership)
        .register(V083MeshSenderNames)
        .register(V084ScopedReactions)
        .register(V085ReactionReceiveJournal)
        .register(V086ChannelMessageClocks)
        .register(V087SingleReaction)
        .register(V088ReactionNormalization)
        .register(V089PinAddedOrder)
        .register(V090DeleteBatchRetry)
        .register(V092SavedPinWatermark)
        .register(V093SavedSyncOutbox)
        .register(V094AnonMembershipOrder)
        .register(v095_saved_delete_tombstones::V095SavedDeleteTombstones)
        .register(v096_saved_delete_outbox::V096SavedDeleteOutbox)
        .register(v097_dm_outbox::V097DmOutbox)
        .register(v098_dm_outbox_sealed_at::V098DmOutboxSealedAt)
        .register(v099_dm_receive_tombstones::V099DmReceiveTombstones)
        .register(v100_drop_message_deletion_token::V100DropMessageDeletionToken)
        .register(v101_attachment_secret_chat::V101AttachmentSecretChat)
        .register(v102_drop_call_room_tokens::V102DropCallRoomTokens)
        .register(v103_secret_chat_peer_device::V103SecretChatPeerDevice)
        .register(v104_reaction_send_clocks::V104ReactionSendClocks)
        .register(v105_drop_legacy_reaction_evidence::V105DropLegacyReactionEvidence)
        .register(v106_drop_anon_group_legacy_lock::V106DropAnonGroupLegacyLock)
        .register(v107_drop_attachment_local_path::V107DropAttachmentLocalPath)
        .register(v108_drop_channel_message_clocks::V108DropChannelMessageClocks)
        .register(v109_drop_mailbox_randomizer::V109DropMailboxRandomizer)
        .register(v110_attachment_secret_chat_known::V110AttachmentSecretChatKnown)
        .register(v111_secret_chat_request_state::V111SecretChatRequestState)
        .register(v112_send_log_group_id::V112SendLogGroupId)
        .register(v113_mesh_account_binding::V113MeshAccountBinding)
        .register(v114_mesh_signal::V114MeshSignal)
        .register(v115_mesh_signal_courier::V115MeshSignalCourier)
        .register(v119_pending_anon_group_invites::V119PendingAnonGroupInvites)
        .register(v116_mesh_private_channels::V116MeshPrivateChannels)
        .register(v117_mesh_conversation_kinds::V117MeshConversationKinds)
        .register(v118_read_horizons::V118ReadHorizons)
        .register(v120_secret_receive_artifacts::V120SecretReceiveArtifacts)
        .register(v121_anon_group_departed_profiles::V121AnonGroupDepartedProfiles)
        .register(v122_secret_receive_control_kinds::V122SecretReceiveControlKinds)
        .register(v123_attachment_cleanup::V123AttachmentCleanup)
        .register(v124_activity_center::V124ActivityCenter)
        .register(v125_contact_names::V125ContactNames)
        .register(v126_activity_outage_resolution::V126ActivityOutageResolution)
        .register(v127_shared_content_categories::V127SharedContentCategories)
        .register(v128_anon_group_invite_epoch::V128AnonGroupInviteEpoch)
        .register(v129_pending_dm_retry::V129PendingDmRetry)
        .register(v131_mesh_archive_packets::V131MeshArchivePackets)
}

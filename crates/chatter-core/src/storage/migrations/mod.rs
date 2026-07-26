//! Database migrations.
//!
//! Migration infrastructure for schema evolution with version tracking.

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
pub mod v059_mesh_favorites;
mod v060_mesh_delivery;
mod v061_mesh_exempt_sender_sent_at;

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
pub use v059_mesh_favorites::V059MeshFavorites;
pub use v060_mesh_delivery::V060MeshDelivery;
pub use v061_mesh_exempt_sender_sent_at::V061MeshExemptSenderSentAt;

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
    /// Creates the schema_version table if it doesn't exist, then applies
    /// any migrations with version > current version in order.
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

        // Get current version
        let current_version: u32 = conn
            .query_row(
                "SELECT COALESCE(MAX(version), 0) FROM schema_version",
                [],
                |row| row.get(0),
            )
            .unwrap_or(0);

        // Sort migrations by version
        let mut sorted_migrations: Vec<_> = self.migrations.iter().collect();
        sorted_migrations.sort_by_key(|m| m.version());

        // Apply pending migrations
        for migration in sorted_migrations {
            let version = migration.version();
            if version > current_version {
                // Execute migration in a transaction
                let tx = conn.unchecked_transaction()?;

                // Execute the up SQL
                tx.execute_batch(migration.up())?;

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
        .register(V059MeshFavorites)
        .register(V060MeshDelivery)
        .register(V061MeshExemptSenderSentAt)
}

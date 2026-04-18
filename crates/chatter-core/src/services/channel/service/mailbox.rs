//! Group mailbox operations for channels.
//!
//! Setup and management of ephemeral group mailboxes (NDSS '21 paper).

use crate::domain::ids::ChannelId;
use crate::mailbox::group::GroupMailboxSetupResult;
use crate::network::grpc::mailbox_client::MailboxClient;
use crate::network::grpc::AnonymousMailboxClient;
use crate::services::current_user::CurrentUserProvider;

use crate::services::channel::error::ChannelError;

use super::ChannelService;

impl ChannelService {
    /// Setup group mailbox for a channel.
    ///
    /// Creates an ephemeral mailbox for receiving group messages (NDSS '21 paper).
    /// Returns the setup result containing the mailbox and announcement proto.
    ///
    /// The caller should send the announcement via `MessageService::send_control_content()`
    /// so other members can learn our mailbox address.
    ///
    /// # Arguments
    ///
    /// * `channel_id` - The channel to setup mailbox for
    ///
    /// # Returns
    ///
    /// * `Ok(Some(result))` - Mailbox created, announcement ready to send
    /// * `Ok(None)` - Skipped (DM or non-encrypted channel)
    /// * `Err(_)` - Setup failed
    pub async fn setup_group_mailbox(
        &self,
        channel_id: &ChannelId,
    ) -> Result<Option<GroupMailboxSetupResult>, ChannelError> {
        let mut mailbox_client = self.mailbox_client();
        let mut anon_mailbox_client = self.anonymous_mailbox_client();
        self.setup_group_mailbox_internal(channel_id, &mut mailbox_client, &mut anon_mailbox_client).await
    }

    /// Internal: Setup group mailbox using existing clients.
    pub(super) async fn setup_group_mailbox_internal(
        &self,
        channel_id: &ChannelId,
        mailbox_client: &mut MailboxClient,
        anon_mailbox_client: &mut AnonymousMailboxClient,
    ) -> Result<Option<GroupMailboxSetupResult>, ChannelError> {
        let _ = self.settings_repo.current_user_id()?;

        tracing::info!(
            channel_id = %channel_id,
            "[MAILBOX_SETUP] Starting group mailbox setup"
        );

        // Check if this is an encrypted channel that needs group mailbox
        let channel = self
            .channel_repo
            .get(channel_id)?
            .ok_or_else(|| ChannelError::ChannelNotFound(*channel_id))?;

        tracing::info!(
            channel_id = %channel_id,
            channel_type = ?channel.channel_type,
            require_encryption = channel.config.require_encryption,
            "[MAILBOX_SETUP] Channel retrieved from DB"
        );

        // Server-trusted channels (Public/Private/DM/SavedMessages) now deliver
        // everything over the device inbox — regular messages via the server
        // message stream, SKDM + control content via per-device sealed sender.
        // The group-mailbox transport added a second polling loop with no
        // additional privacy (server already knows the membership), so it has
        // been retired for all server-managed channel types.
        //
        // Anon groups (NDSS'21) register their own mailbox via
        // `AnonGroupService::create_group_mailbox` — they do NOT flow through
        // this path.
        //
        // Kept as a no-op to preserve call-site ergonomics during migration.
        let _ = (channel, mailbox_client, anon_mailbox_client);
        tracing::debug!(
            channel_id = %channel_id,
            "Group mailbox setup retired for server-managed channels; skipping"
        );
        Ok(None)
    }

}

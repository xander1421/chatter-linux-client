//! Group mailbox operations for channels.
//!
//! Setup and management of ephemeral group mailboxes (NDSS '21 paper).

use crate::domain::channel::ChannelType;
use crate::domain::ids::ChannelId;
use crate::mailbox::group::GroupMailboxSetupResult;
use crate::network::grpc::mailbox_client::MailboxClient;
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
        self.setup_group_mailbox_internal(channel_id, &mut mailbox_client).await
    }

    /// Internal: Setup group mailbox using an existing client.
    pub(super) async fn setup_group_mailbox_internal(
        &self,
        channel_id: &ChannelId,
        mailbox_client: &mut MailboxClient,
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

        // Skip DMs - they use two-way anonymous mailboxes
        if channel.channel_type == ChannelType::DirectMessage {
            tracing::debug!(
                channel_id = %channel_id,
                "Skipping group mailbox setup for DM channel"
            );
            return Ok(None);
        }

        // Skip non-encrypted channels
        if !channel.config.require_encryption {
            tracing::info!(
                channel_id = %channel_id,
                "[MAILBOX_SETUP] Skipping - require_encryption is false"
            );
            return Ok(None);
        }

        tracing::info!(
            channel_id = %channel_id,
            "[MAILBOX_SETUP] Channel requires encryption, proceeding with mailbox setup"
        );

        // Setup the ephemeral mailbox via GroupCoordinator
        match self.group_coordinator.setup_group_mailbox(channel_id, mailbox_client).await {
            Ok(setup_result) => {
                tracing::info!(
                    channel_id = %channel_id,
                    mailbox_id = %setup_result.mailbox.id,
                    "Created group mailbox"
                );
                Ok(Some(setup_result))
            }
            Err(e) => {
                tracing::warn!(
                    channel_id = %channel_id,
                    error = %e,
                    "Failed to setup group mailbox"
                );
                Err(ChannelError::InvalidInput(format!(
                    "Failed to setup group mailbox: {}",
                    e
                )))
            }
        }
    }

    /// Store a member's mailbox public key for a group channel.
    ///
    /// Called when receiving a group mailbox announcement from a peer.
    /// Returns true if this was a new mailbox (not already known).
    pub fn store_member_mailbox(
        &self,
        channel_id: &ChannelId,
        member_id: &crate::domain::ids::UserId,
        mailbox_pk: &[u8; 32],
    ) -> Result<bool, ChannelError> {
        use crate::mailbox::group::MemberMailbox;

        let already_known = self.group_coordinator
            .mailbox_manager()
            .load_group_state(channel_id)
            .ok()
            .flatten()
            .map(|state| state.get_member_mailbox(member_id).is_some())
            .unwrap_or(false);

        let member_mailbox = MemberMailbox::new(
            *member_id,
            *mailbox_pk,
            Some(chrono::Utc::now() + chrono::Duration::days(7)),
        );

        self.group_coordinator
            .mailbox_manager()
            .save_member_mailbox(channel_id, &member_mailbox)
            .map_err(|e| ChannelError::InvalidInput(format!("Failed to store member mailbox: {}", e)))?;

        Ok(!already_known)
    }
}
